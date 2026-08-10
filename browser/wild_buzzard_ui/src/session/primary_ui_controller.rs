use super::{
    BrowserSession, BrowserTabId, BrowserWindowId, EnginePort, MAX_PRIMARY_UI_LABEL_BYTES,
    MAX_PRIMARY_UI_SCROLL_ROWS, PrimaryReloadStopMode, PrimarySiteIdentityKind, PrimaryUiAction,
    PrimaryUiActionBinding, PrimaryUiActionOutcome, PrimaryUiAvailability, PrimaryUiControl,
    PrimaryUiControlSnapshot, PrimaryUiDirection, PrimaryUiElementId, PrimaryUiFocus,
    PrimaryUiInteraction, PrimaryUiLayout, PrimaryUiMoveDirection, PrimaryUiPanel,
    PrimaryUiPanelItemAction, PrimaryUiPanelItemId, PrimaryUiPanelItemSnapshot,
    PrimaryUiPanelSnapshot, PrimaryUiRevision, PrimaryUiRole, PrimaryUiSemanticNode,
    PrimaryUiSnapshot, PrimaryUiTabSnapshot, SessionError, SessionFailure, TabFocus, TabState,
    WindowState,
};

#[derive(Clone, Copy)]
struct PrimaryControlFacts {
    availability: PrimaryUiAvailability,
    interaction: PrimaryUiInteraction,
    reload_stop_mode: Option<PrimaryReloadStopMode>,
    site_identity: Option<PrimarySiteIdentityKind>,
}

#[derive(Clone, Copy)]
enum PrimaryFocusRepair {
    None,
    Content,
    Page {
        close_panel: bool,
    },
    Panel {
        focus: PrimaryUiFocus,
        selected: Option<PrimaryUiPanelItemId>,
        all_tabs_scroll: usize,
    },
}

impl<E: EnginePort> BrowserSession<E> {
    /// Current primary-UI revision for one live window.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::UnknownWindow`] when `window` is not live.
    pub fn primary_ui_revision(
        &self,
        window: BrowserWindowId,
    ) -> Result<PrimaryUiRevision, SessionError> {
        Ok(self
            .windows
            .get(&window)
            .ok_or(SessionError::UnknownWindow(window))?
            .primary_ui
            .revision)
    }

    /// Current canonical primary-chrome inline direction for one live window.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::UnknownWindow`] when `window` is not live.
    pub fn primary_ui_direction(
        &self,
        window: BrowserWindowId,
    ) -> Result<PrimaryUiDirection, SessionError> {
        Ok(self
            .windows
            .get(&window)
            .ok_or(SessionError::UnknownWindow(window))?
            .primary_ui
            .direction)
    }

    /// Projects canonical session state into an immutable functional primary UI.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] for an unknown window/tab or bounded snapshot
    /// allocation failure. No engine or native state is changed.
    pub fn primary_ui_snapshot(
        &self,
        window_id: BrowserWindowId,
    ) -> Result<PrimaryUiSnapshot, SessionError> {
        let window = self
            .windows
            .get(&window_id)
            .ok_or(SessionError::UnknownWindow(window_id))?;
        if !valid_primary_focus(window.primary_ui.focus) {
            return Err(SessionError::InvalidPrimaryUiFocus {
                focus: window.primary_ui.focus,
            });
        }
        let active = self
            .tabs
            .get(&window.active)
            .ok_or(SessionError::UnknownTab(window.active))?;
        let facts = self.primary_control_facts(window, active);

        let mut controls = Vec::new();
        controls
            .try_reserve_exact(PrimaryUiControl::ALL.len())
            .map_err(|_| SessionError::PrimaryUiResource {
                detail: "could not reserve primary control snapshot",
            })?;
        for control in PrimaryUiControl::ALL {
            let control_facts = facts[usize::from(control as u8)];
            controls.push(PrimaryUiControlSnapshot {
                control,
                name: primary_control_name(control, control_facts).into(),
                availability: control_facts.availability,
                interaction: control_facts.interaction,
                visible: window.primary_ui.layout.visible().contains(control),
                overflowed: window.primary_ui.layout.overflowed().contains(control),
                expanded: window
                    .primary_ui
                    .panel
                    .is_some_and(|panel| panel.anchor() == control),
                focused: primary_focus_matches_control(window.primary_ui.focus, control),
                reload_stop_mode: control_facts.reload_stop_mode,
                site_identity: control_facts.site_identity,
            });
        }

        let mut tabs = Vec::new();
        tabs.try_reserve_exact(window.tabs.len())
            .map_err(|_| SessionError::PrimaryUiResource {
                detail: "could not reserve primary tab snapshot",
            })?;
        for tab_id in window.tabs.iter().copied() {
            let tab = self
                .tabs
                .get(&tab_id)
                .ok_or(SessionError::UnknownTab(tab_id))?;
            tabs.push(PrimaryUiTabSnapshot {
                tab: tab_id,
                name: primary_tab_name(tab),
                selected: tab_id == window.active,
                loading: tab.loading.is_some(),
                focused: window.primary_ui.focus == PrimaryUiFocus::Tab(tab_id),
                close_availability: PrimaryUiAvailability::Enabled,
            });
        }

        let panel = window
            .primary_ui
            .panel
            .map(|panel| Self::primary_panel_snapshot(window, panel, &controls, &tabs))
            .transpose()?;
        let semantics = primary_semantic_nodes(window, &controls, &tabs, panel.as_ref())?;
        Ok(PrimaryUiSnapshot {
            window: window_id,
            revision: window.primary_ui.revision,
            direction: window.primary_ui.direction,
            focus: window.primary_ui.focus,
            controls: controls.into_boxed_slice(),
            tabs: tabs.into_boxed_slice(),
            panel,
            semantics,
        })
    }

    /// Changes the canonical inline direction used by geometry and keyboard navigation.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] when the window is unknown or its revision is exhausted.
    pub fn set_primary_ui_direction(
        &mut self,
        window: BrowserWindowId,
        direction: PrimaryUiDirection,
    ) -> Result<PrimaryUiRevision, SessionError> {
        self.ensure_running()?;
        let state = &mut self
            .windows
            .get_mut(&window)
            .ok_or(SessionError::UnknownWindow(window))?
            .primary_ui;
        if state.direction == direction {
            return Ok(state.revision);
        }
        state.direction = direction;
        self.bump_primary_ui_revision(window)
    }

    /// Installs A4's exact resolved visible/overflow membership.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] when the window is unknown or its revision is exhausted.
    pub fn set_primary_ui_layout(
        &mut self,
        window: BrowserWindowId,
        layout: PrimaryUiLayout,
    ) -> Result<PrimaryUiRevision, SessionError> {
        self.ensure_running()?;
        let state = &mut self
            .windows
            .get_mut(&window)
            .ok_or(SessionError::UnknownWindow(window))?
            .primary_ui;
        if state.layout == layout {
            return Ok(state.revision);
        }
        let previous_layout = state.layout;
        state.layout = layout;
        let panel_lost_anchor = state
            .panel
            .is_some_and(|panel| !layout.visible().contains(panel.anchor()));
        let panel_lost_capacity = state.panel.is_some() && layout.panel_row_capacity() == 0;
        let overflow_membership_changed = state.panel == Some(PrimaryUiPanel::Overflow)
            && previous_layout.overflowed() != layout.overflowed();
        if panel_lost_anchor || panel_lost_capacity || overflow_membership_changed {
            state.panel = None;
            state.panel_selected = None;
            state.focus = PrimaryUiFocus::Page;
        }
        if let PrimaryUiFocus::Control(control) = state.focus
            && !layout.visible().contains(control)
        {
            state.focus = if layout.visible().contains(PrimaryUiControl::Overflow) {
                PrimaryUiFocus::Control(PrimaryUiControl::Overflow)
            } else {
                PrimaryUiFocus::Page
            };
        }
        self.bump_primary_ui_revision(window)
    }

    /// Dispatches one action only against the exact current canonical revision.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] when an enabled real action fails or the session
    /// transitions terminal. Stale and disabled actions are typed outcomes and
    /// perform no engine effect.
    pub fn dispatch_primary_ui_action(
        &mut self,
        window: BrowserWindowId,
        expected_revision: PrimaryUiRevision,
        action: PrimaryUiAction,
    ) -> Result<PrimaryUiActionOutcome, SessionError> {
        self.ensure_running()?;
        let current = self.primary_ui_revision(window)?;
        if current != expected_revision {
            return Ok(PrimaryUiActionOutcome::Stale {
                expected: expected_revision,
                current,
            });
        }
        match action {
            PrimaryUiAction::FocusPage => {
                let tab = self.active_tab(window)?;
                self.focus_content(tab).map(PrimaryUiActionOutcome::Command)
            }
            PrimaryUiAction::InvokeControl(control) => {
                self.invoke_primary_control(window, control, false)
            }
            PrimaryUiAction::ActivateTab(tab) => {
                if !self.window_contains_tab(window, tab)? {
                    return Ok(PrimaryUiActionOutcome::Disabled(PrimaryUiElementId::Tab(
                        tab,
                    )));
                }
                let outcome = self.activate_tab(tab)?;
                self.close_primary_panel_after_command(window)?;
                Ok(PrimaryUiActionOutcome::Command(outcome))
            }
            PrimaryUiAction::CloseTab(tab) => {
                if !self.window_contains_tab(window, tab)? {
                    return Ok(PrimaryUiActionOutcome::Disabled(
                        PrimaryUiElementId::TabClose(tab),
                    ));
                }
                let outcome = self.close_tab(tab)?;
                self.close_primary_panel_after_command(window)?;
                Ok(PrimaryUiActionOutcome::Command(outcome))
            }
            PrimaryUiAction::ActivatePanelItem(item) => {
                self.activate_primary_panel_item(window, item)
            }
            PrimaryUiAction::DismissPanel => self.dismiss_primary_panel(window),
            PrimaryUiAction::MoveDocumentFocus(direction) => {
                self.move_primary_document_focus(window, direction)
            }
            PrimaryUiAction::MoveFocus(direction) => {
                if self
                    .windows
                    .get(&window)
                    .ok_or(SessionError::UnknownWindow(window))?
                    .primary_ui
                    .panel
                    .is_some()
                {
                    self.move_primary_panel_selection(window, direction)
                } else {
                    self.move_primary_focus(window, direction, false)
                }
            }
            PrimaryUiAction::MoveToolbarFocus(direction) => {
                self.move_primary_focus(window, direction, true)
            }
            PrimaryUiAction::MovePanelSelection(direction) => {
                self.move_primary_panel_selection(window, direction)
            }
            PrimaryUiAction::ScrollPanel { direction, rows } => {
                self.scroll_primary_panel(window, direction, rows)
            }
            PrimaryUiAction::ActivateFocused => {
                let focus = self
                    .windows
                    .get(&window)
                    .ok_or(SessionError::UnknownWindow(window))?
                    .primary_ui
                    .focus;
                let action = match focus {
                    PrimaryUiFocus::Tab(tab) => PrimaryUiAction::ActivateTab(tab),
                    PrimaryUiFocus::Control(control) => PrimaryUiAction::InvokeControl(control),
                    PrimaryUiFocus::PanelItem(item) => PrimaryUiAction::ActivatePanelItem(item),
                    PrimaryUiFocus::Page | PrimaryUiFocus::AddressBar => {
                        return Ok(PrimaryUiActionOutcome::NoChange);
                    }
                };
                self.dispatch_primary_ui_action(window, expected_revision, action)
            }
        }
    }

    /// Dispatches an opaque action binding produced by an exact UI snapshot.
    ///
    /// Stale revisions and controls that became unavailable are suppressed
    /// before any engine effect.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError`] under the same conditions as
    /// [`Self::dispatch_primary_ui_action`].
    pub fn dispatch_primary_ui_binding(
        &mut self,
        binding: PrimaryUiActionBinding,
    ) -> Result<PrimaryUiActionOutcome, SessionError> {
        let window = binding.window();
        let current = self.primary_ui_revision(window)?;
        if current != binding.revision() {
            return Ok(PrimaryUiActionOutcome::Stale {
                expected: binding.revision(),
                current,
            });
        }
        let snapshot = self.primary_ui_snapshot(window)?;
        let scroll_binding = match binding.action() {
            PrimaryUiAction::ScrollPanel { direction, rows } => {
                snapshot.bind_panel_scroll(direction, rows)
            }
            _ => None,
        };
        if snapshot.bind_action(binding.source()) != Some(binding)
            && snapshot.bind_panel_dismissal() != Some(binding)
            && scroll_binding != Some(binding)
        {
            return Ok(PrimaryUiActionOutcome::Disabled(binding.source()));
        }
        self.dispatch_primary_ui_action(window, binding.revision(), binding.action())
    }

    pub(crate) fn bump_primary_ui_revision(
        &mut self,
        window: BrowserWindowId,
    ) -> Result<PrimaryUiRevision, SessionError> {
        self.repair_primary_focus(window)?;
        let bumped = self
            .windows
            .get_mut(&window)
            .ok_or(SessionError::UnknownWindow(window))?
            .primary_ui
            .bump();
        if !bumped {
            return self.fail(SessionFailure::PrimaryUiRevisionExhausted { window });
        }
        self.primary_ui_revision(window)
    }

    pub(crate) fn bump_primary_ui_for_tab(
        &mut self,
        tab: BrowserTabId,
    ) -> Result<PrimaryUiRevision, SessionError> {
        let window = self
            .tabs
            .get(&tab)
            .ok_or(SessionError::UnknownTab(tab))?
            .window;
        self.bump_primary_ui_revision(window)
    }

    fn repair_primary_focus(&mut self, window: BrowserWindowId) -> Result<(), SessionError> {
        let repair = {
            let window_state = self
                .windows
                .get(&window)
                .ok_or(SessionError::UnknownWindow(window))?;
            let active = self
                .tabs
                .get(&window_state.active)
                .ok_or(SessionError::UnknownTab(window_state.active))?;
            let facts = self.primary_control_facts(window_state, active);
            if window_state.primary_ui.focus == PrimaryUiFocus::AddressBar
                && active.focus != TabFocus::Address
            {
                PrimaryFocusRepair::Page {
                    close_panel: window_state.primary_ui.panel.is_some(),
                }
            } else if window_state.primary_ui.focus != PrimaryUiFocus::AddressBar
                && active.focus == TabFocus::Address
            {
                PrimaryFocusRepair::Content
            } else if let Some(panel) = window_state.primary_ui.panel
                && (!window_state
                    .primary_ui
                    .layout
                    .visible()
                    .contains(panel.anchor())
                    || !facts[usize::from(panel.anchor() as u8)]
                        .availability
                        .is_enabled()
                    || window_state.primary_ui.layout.panel_row_capacity() == 0)
            {
                PrimaryFocusRepair::Page { close_panel: true }
            } else {
                match window_state.primary_ui.focus {
                    PrimaryUiFocus::Control(PrimaryUiControl::AddressBar) => {
                        return Err(SessionError::InvalidPrimaryUiFocus {
                            focus: window_state.primary_ui.focus,
                        });
                    }
                    PrimaryUiFocus::Control(control)
                        if !window_state.primary_ui.layout.visible().contains(control)
                            || !facts[usize::from(control as u8)].availability.is_enabled() =>
                    {
                        PrimaryFocusRepair::Page {
                            close_panel: window_state.primary_ui.panel.is_some(),
                        }
                    }
                    PrimaryUiFocus::PanelItem(item) => match window_state.primary_ui.panel {
                        None => PrimaryFocusRepair::Page { close_panel: false },
                        Some(panel) => {
                            if window_state.primary_ui.panel_selected == Some(item)
                                && primary_panel_item_is_enabled(window_state, panel, item, &facts)
                            {
                                PrimaryFocusRepair::None
                            } else {
                                let selected =
                                    first_enabled_primary_panel_item(window_state, panel, &facts);
                                let focus = selected.map_or(
                                    PrimaryUiFocus::Control(panel.anchor()),
                                    PrimaryUiFocus::PanelItem,
                                );
                                let all_tabs_scroll = selected
                                    .and_then(|selected| {
                                        primary_panel_item_index(window_state, panel, selected)
                                    })
                                    .map_or(0, |index| {
                                        index.saturating_add(1).saturating_sub(
                                            window_state.primary_ui.layout.panel_row_capacity(),
                                        )
                                    });
                                PrimaryFocusRepair::Panel {
                                    focus,
                                    selected,
                                    all_tabs_scroll,
                                }
                            }
                        }
                    },
                    PrimaryUiFocus::Page
                    | PrimaryUiFocus::Tab(_)
                    | PrimaryUiFocus::AddressBar
                    | PrimaryUiFocus::Control(_) => PrimaryFocusRepair::None,
                }
            }
        };
        self.apply_primary_focus_repair(window, repair)
    }

    fn apply_primary_focus_repair(
        &mut self,
        window: BrowserWindowId,
        repair: PrimaryFocusRepair,
    ) -> Result<(), SessionError> {
        let (focus, selected, scroll, close_panel) = match repair {
            PrimaryFocusRepair::None => return Ok(()),
            PrimaryFocusRepair::Content => {
                let tab = self
                    .windows
                    .get(&window)
                    .ok_or(SessionError::UnknownWindow(window))?
                    .active;
                let tab_state = self
                    .tabs
                    .get_mut(&tab)
                    .ok_or(SessionError::UnknownTab(tab))?;
                tab_state.focus = TabFocus::Content;
                tab_state.address.clear_preedit();
                return Ok(());
            }
            PrimaryFocusRepair::Page { close_panel } => {
                (PrimaryUiFocus::Page, None, 0, close_panel)
            }
            PrimaryFocusRepair::Panel {
                focus,
                selected,
                all_tabs_scroll,
            } => (focus, selected, all_tabs_scroll, false),
        };
        let tab = self
            .windows
            .get(&window)
            .ok_or(SessionError::UnknownWindow(window))?
            .active;
        let tab_state = self
            .tabs
            .get_mut(&tab)
            .ok_or(SessionError::UnknownTab(tab))?;
        tab_state.focus = TabFocus::Content;
        tab_state.address.clear_preedit();
        let state = &mut self
            .windows
            .get_mut(&window)
            .ok_or(SessionError::UnknownWindow(window))?
            .primary_ui;
        state.focus = focus;
        state.panel_selected = selected;
        state.all_tabs_scroll = scroll;
        if close_panel {
            state.panel = None;
        }
        Ok(())
    }

    fn primary_control_facts(
        &self,
        window: &WindowState,
        active: &TabState,
    ) -> [PrimaryControlFacts; PrimaryUiControl::ALL.len()] {
        let history_index = active.history_index;
        let can_back = history_index.is_some_and(|index| index > 0);
        let can_forward = history_index.is_some_and(|index| index + 1 < active.history.len());
        let loading = active.loading.is_some();
        let displayed_address = active.live_navigation.and_then(|navigation| {
            active
                .history
                .iter()
                .find(|entry| entry.navigation == navigation)
                .map(|entry| entry.address.as_ref())
        });
        let identity = classify_site_identity(displayed_address);
        let popup_available = window.primary_ui.layout.panel_row_capacity() != 0;
        let identity_enabled =
            displayed_address.is_some() && !active.address.is_dirty() && popup_available;
        let new_tab_enabled = window.tabs.len() < self.limits.max_tabs_per_window
            && self.tabs.len() < self.limits.max_total_tabs;
        let overflow_present = window
            .primary_ui
            .layout
            .overflowed()
            .iter()
            .next()
            .is_some();
        [
            facts(can_back, PrimaryUiInteraction::Invoke, None, None),
            facts(can_forward, PrimaryUiInteraction::Invoke, None, None),
            facts(
                if loading {
                    !active.stop_requested
                } else {
                    history_index.is_some()
                },
                PrimaryUiInteraction::Invoke,
                Some(if loading {
                    PrimaryReloadStopMode::Stop
                } else {
                    PrimaryReloadStopMode::Reload
                }),
                None,
            ),
            facts(
                identity_enabled,
                PrimaryUiInteraction::TogglePanel,
                None,
                Some(identity),
            ),
            facts(true, PrimaryUiInteraction::Edit, None, None),
            facts(new_tab_enabled, PrimaryUiInteraction::Invoke, None, None),
            facts(
                popup_available,
                PrimaryUiInteraction::TogglePanel,
                None,
                None,
            ),
            facts(
                popup_available,
                PrimaryUiInteraction::TogglePanel,
                None,
                None,
            ),
            facts(
                overflow_present && popup_available,
                PrimaryUiInteraction::TogglePanel,
                None,
                None,
            ),
        ]
    }

    fn primary_panel_snapshot(
        window: &WindowState,
        panel: PrimaryUiPanel,
        controls: &[PrimaryUiControlSnapshot],
        tabs: &[PrimaryUiTabSnapshot],
    ) -> Result<PrimaryUiPanelSnapshot, SessionError> {
        let all_items = primary_panel_items(window, panel, controls, tabs)?;
        let total_rows = all_items.len();
        let capacity = window.primary_ui.layout.panel_row_capacity();
        let selected_index = window
            .primary_ui
            .panel_selected
            .and_then(|selected| all_items.iter().position(|item| item.id == selected));
        let maximum_start = total_rows.saturating_sub(capacity);
        let mut start = if panel == PrimaryUiPanel::AllTabs {
            window.primary_ui.all_tabs_scroll.min(maximum_start)
        } else {
            0
        };
        if let Some(selected) = selected_index {
            if selected < start {
                start = selected;
            } else if selected >= start.saturating_add(capacity) {
                start = selected.saturating_add(1).saturating_sub(capacity);
            }
        }
        Ok(PrimaryUiPanelSnapshot {
            panel,
            anchor: panel.anchor(),
            items: all_items.into_boxed_slice(),
            selected: window.primary_ui.panel_selected,
            scroll_offset: start,
            visible_capacity: capacity,
            total_rows,
        })
    }

    fn invoke_primary_control(
        &mut self,
        window: BrowserWindowId,
        control: PrimaryUiControl,
        from_panel: bool,
    ) -> Result<PrimaryUiActionOutcome, SessionError> {
        let snapshot = self.primary_ui_snapshot(window)?;
        let control_snapshot = snapshot
            .controls
            .iter()
            .find(|candidate| candidate.control == control)
            .expect("snapshot contains every fixed primary control");
        if !control_snapshot.availability.is_enabled()
            || (!control_snapshot.visible && (!from_panel || !control_snapshot.overflowed))
        {
            return Ok(PrimaryUiActionOutcome::Disabled(
                PrimaryUiElementId::Control(control),
            ));
        }
        let tab = self
            .windows
            .get(&window)
            .ok_or(SessionError::UnknownWindow(window))?
            .active;
        match control {
            PrimaryUiControl::Back => self
                .go_history(tab, -1)
                .map(PrimaryUiActionOutcome::Command),
            PrimaryUiControl::Forward => {
                self.go_history(tab, 1).map(PrimaryUiActionOutcome::Command)
            }
            PrimaryUiControl::ReloadStop => match control_snapshot.reload_stop_mode {
                Some(PrimaryReloadStopMode::Reload) => {
                    self.reload(tab).map(PrimaryUiActionOutcome::Command)
                }
                Some(PrimaryReloadStopMode::Stop) => {
                    self.stop(tab).map(PrimaryUiActionOutcome::Command)
                }
                None => Ok(PrimaryUiActionOutcome::Disabled(
                    PrimaryUiElementId::Control(control),
                )),
            },
            PrimaryUiControl::SiteIdentity => {
                self.toggle_primary_panel(window, PrimaryUiPanel::SiteIdentity)
            }
            PrimaryUiControl::AddressBar => self
                .focus_address(window)
                .map(PrimaryUiActionOutcome::Command),
            PrimaryUiControl::NewTab => self.open_tab(window).map(PrimaryUiActionOutcome::Command),
            PrimaryUiControl::AllTabs => self.toggle_primary_panel(window, PrimaryUiPanel::AllTabs),
            PrimaryUiControl::ApplicationMenu => {
                self.toggle_primary_panel(window, PrimaryUiPanel::ApplicationMenu)
            }
            PrimaryUiControl::Overflow => {
                self.toggle_primary_panel(window, PrimaryUiPanel::Overflow)
            }
        }
    }

    fn toggle_primary_panel(
        &mut self,
        window: BrowserWindowId,
        panel: PrimaryUiPanel,
    ) -> Result<PrimaryUiActionOutcome, SessionError> {
        let snapshot = self.primary_ui_snapshot(window)?;
        let anchor = panel.anchor();
        let control = snapshot
            .controls
            .iter()
            .find(|candidate| candidate.control == anchor)
            .expect("snapshot contains panel anchor");
        if !control.visible || !control.availability.is_enabled() {
            return Ok(PrimaryUiActionOutcome::Disabled(
                PrimaryUiElementId::Control(anchor),
            ));
        }
        let closing = snapshot
            .panel
            .as_ref()
            .is_some_and(|open| open.panel == panel);
        if closing {
            // Popups own keyboard focus. They must never leave the hidden URL
            // editor accepting text or IME composition behind the panel.
            self.set_active_tab_focus(window, TabFocus::Content)?;
            let state = &mut self
                .windows
                .get_mut(&window)
                .ok_or(SessionError::UnknownWindow(window))?
                .primary_ui;
            state.panel = None;
            state.panel_selected = None;
            state.focus = if anchor == PrimaryUiControl::AddressBar {
                PrimaryUiFocus::AddressBar
            } else {
                PrimaryUiFocus::Control(anchor)
            };
            self.bump_primary_ui_revision(window)?;
            return Ok(PrimaryUiActionOutcome::PanelChanged(None));
        }

        let items = primary_panel_items(
            self.windows
                .get(&window)
                .ok_or(SessionError::UnknownWindow(window))?,
            panel,
            &snapshot.controls,
            &snapshot.tabs,
        )?;
        let selected = if panel == PrimaryUiPanel::AllTabs {
            let active = self
                .windows
                .get(&window)
                .ok_or(SessionError::UnknownWindow(window))?
                .active;
            Some(PrimaryUiPanelItemId::AllTabsTab(active))
        } else {
            items
                .iter()
                .find(|item| item.availability.is_enabled())
                .map(|item| item.id)
        };
        let selected_index = selected.and_then(|id| items.iter().position(|item| item.id == id));
        // Do this only after the fallible row snapshot is complete, keeping a
        // resource rejection non-mutating.
        self.set_active_tab_focus(window, TabFocus::Content)?;
        let state = &mut self
            .windows
            .get_mut(&window)
            .ok_or(SessionError::UnknownWindow(window))?
            .primary_ui;
        state.panel = Some(panel);
        state.panel_selected = selected;
        state.all_tabs_scroll = selected_index.map_or(0, |index| {
            index
                .saturating_add(1)
                .saturating_sub(state.layout.panel_row_capacity())
        });
        state.focus = selected.map_or(PrimaryUiFocus::Control(anchor), PrimaryUiFocus::PanelItem);
        self.bump_primary_ui_revision(window)?;
        Ok(PrimaryUiActionOutcome::PanelChanged(Some(panel)))
    }

    fn dismiss_primary_panel(
        &mut self,
        window: BrowserWindowId,
    ) -> Result<PrimaryUiActionOutcome, SessionError> {
        let state = &mut self
            .windows
            .get_mut(&window)
            .ok_or(SessionError::UnknownWindow(window))?
            .primary_ui;
        let Some(panel) = state.panel else {
            if matches!(
                state.focus,
                PrimaryUiFocus::Control(_) | PrimaryUiFocus::Tab(_)
            ) {
                let previous = state.focus;
                state.focus = PrimaryUiFocus::Page;
                self.set_active_tab_focus(window, TabFocus::Content)?;
                self.bump_primary_ui_revision(window)?;
                return Ok(PrimaryUiActionOutcome::FocusChanged {
                    previous,
                    current: PrimaryUiFocus::Page,
                });
            }
            return Ok(PrimaryUiActionOutcome::NoChange);
        };
        state.panel = None;
        state.panel_selected = None;
        state.focus = PrimaryUiFocus::Control(panel.anchor());
        self.bump_primary_ui_revision(window)?;
        Ok(PrimaryUiActionOutcome::PanelChanged(None))
    }

    fn close_primary_panel_after_command(
        &mut self,
        window: BrowserWindowId,
    ) -> Result<(), SessionError> {
        if !self.windows.contains_key(&window) {
            return Ok(());
        }
        let restored_focus = self.active_tab_primary_focus(window)?;
        let Some(state) = self
            .windows
            .get_mut(&window)
            .map(|window| &mut window.primary_ui)
        else {
            return Ok(());
        };
        if state.panel.is_none() {
            return Ok(());
        }
        state.panel = None;
        state.panel_selected = None;
        state.focus = restored_focus;
        self.bump_primary_ui_revision(window)?;
        Ok(())
    }

    fn activate_primary_panel_item(
        &mut self,
        window: BrowserWindowId,
        item: PrimaryUiPanelItemId,
    ) -> Result<PrimaryUiActionOutcome, SessionError> {
        let snapshot = self.primary_ui_snapshot(window)?;
        let Some(panel) = snapshot.panel.as_ref() else {
            return Ok(PrimaryUiActionOutcome::Disabled(
                PrimaryUiElementId::PanelItem(item),
            ));
        };
        let controls = snapshot.controls.as_ref();
        let tabs = snapshot.tabs.as_ref();
        let all_items = primary_panel_items(
            self.windows
                .get(&window)
                .ok_or(SessionError::UnknownWindow(window))?,
            panel.panel,
            controls,
            tabs,
        )?;
        let Some(row) = all_items.iter().find(|candidate| candidate.id == item) else {
            return Ok(PrimaryUiActionOutcome::Disabled(
                PrimaryUiElementId::PanelItem(item),
            ));
        };
        if !row.availability.is_enabled() {
            return Ok(PrimaryUiActionOutcome::Disabled(
                PrimaryUiElementId::PanelItem(item),
            ));
        }
        match row.action {
            PrimaryUiPanelItemAction::None => Ok(PrimaryUiActionOutcome::Disabled(
                PrimaryUiElementId::PanelItem(item),
            )),
            PrimaryUiPanelItemAction::ActivateTab(tab) => {
                let outcome = self.activate_tab(tab)?;
                self.close_primary_panel_after_command(window)?;
                Ok(PrimaryUiActionOutcome::Command(outcome))
            }
            PrimaryUiPanelItemAction::InvokeControl(control) => {
                let outcome = self.invoke_primary_control(window, control, true)?;
                if matches!(outcome, PrimaryUiActionOutcome::Command(_)) {
                    self.close_primary_panel_after_command(window)?;
                }
                Ok(outcome)
            }
            PrimaryUiPanelItemAction::CloseActiveTab => {
                let tab = self
                    .windows
                    .get(&window)
                    .ok_or(SessionError::UnknownWindow(window))?
                    .active;
                let outcome = self.close_tab(tab)?;
                self.close_primary_panel_after_command(window)?;
                Ok(PrimaryUiActionOutcome::Command(outcome))
            }
        }
    }

    fn move_primary_focus(
        &mut self,
        window: BrowserWindowId,
        direction: PrimaryUiMoveDirection,
        toolbar_only: bool,
    ) -> Result<PrimaryUiActionOutcome, SessionError> {
        let snapshot = self.primary_ui_snapshot(window)?;
        let effective_direction =
            if toolbar_only && snapshot.direction == PrimaryUiDirection::RightToLeft {
                reverse_direction(direction)
            } else {
                direction
            };
        let mut candidates = primary_focus_candidates(&snapshot, toolbar_only)?;
        if candidates.is_empty() {
            return Ok(PrimaryUiActionOutcome::NoChange);
        }
        let previous = snapshot.focus;
        let current_index = candidates
            .iter()
            .position(|candidate| *candidate == previous);
        let next_index = match (current_index, effective_direction) {
            (Some(index), PrimaryUiMoveDirection::Forward) => (index + 1) % candidates.len(),
            (Some(0) | None, PrimaryUiMoveDirection::Backward) => candidates.len() - 1,
            (Some(index), PrimaryUiMoveDirection::Backward) => index - 1,
            (None, PrimaryUiMoveDirection::Forward) => 0,
        };
        let current = candidates.swap_remove(next_index);
        if current == previous {
            return Ok(PrimaryUiActionOutcome::NoChange);
        }
        self.set_primary_focus(window, current)?;
        self.bump_primary_ui_revision(window)?;
        Ok(PrimaryUiActionOutcome::FocusChanged { previous, current })
    }

    fn move_primary_document_focus(
        &mut self,
        window: BrowserWindowId,
        _direction: PrimaryUiMoveDirection,
    ) -> Result<PrimaryUiActionOutcome, SessionError> {
        let previous = self
            .windows
            .get(&window)
            .ok_or(SessionError::UnknownWindow(window))?
            .primary_ui
            .focus;
        let current = if previous == PrimaryUiFocus::Page {
            PrimaryUiFocus::AddressBar
        } else {
            PrimaryUiFocus::Page
        };
        self.set_primary_focus(window, current)?;
        let state = &mut self
            .windows
            .get_mut(&window)
            .ok_or(SessionError::UnknownWindow(window))?
            .primary_ui;
        state.panel = None;
        state.panel_selected = None;
        self.bump_primary_ui_revision(window)?;
        Ok(PrimaryUiActionOutcome::FocusChanged { previous, current })
    }

    fn move_primary_panel_selection(
        &mut self,
        window: BrowserWindowId,
        direction: PrimaryUiMoveDirection,
    ) -> Result<PrimaryUiActionOutcome, SessionError> {
        let snapshot = self.primary_ui_snapshot(window)?;
        let Some(panel) = snapshot.panel.as_ref() else {
            return Ok(PrimaryUiActionOutcome::NoChange);
        };
        let all_items = primary_panel_items(
            self.windows
                .get(&window)
                .ok_or(SessionError::UnknownWindow(window))?,
            panel.panel,
            &snapshot.controls,
            &snapshot.tabs,
        )?;
        let enabled: Vec<_> = all_items
            .iter()
            .filter(|item| item.availability.is_enabled())
            .map(|item| item.id)
            .collect();
        if enabled.is_empty() {
            return Ok(PrimaryUiActionOutcome::NoChange);
        }
        let previous_item = self
            .windows
            .get(&window)
            .ok_or(SessionError::UnknownWindow(window))?
            .primary_ui
            .panel_selected;
        let current_index =
            previous_item.and_then(|item| enabled.iter().position(|id| *id == item));
        let next_index = match (current_index, direction) {
            (Some(index), PrimaryUiMoveDirection::Forward) => (index + 1) % enabled.len(),
            (Some(0) | None, PrimaryUiMoveDirection::Backward) => enabled.len() - 1,
            (Some(index), PrimaryUiMoveDirection::Backward) => index - 1,
            (None, PrimaryUiMoveDirection::Forward) => 0,
        };
        let next = enabled[next_index];
        if previous_item == Some(next) {
            return Ok(PrimaryUiActionOutcome::NoChange);
        }
        let all_index = all_items
            .iter()
            .position(|item| item.id == next)
            .expect("enabled item came from all panel rows");
        let state = &mut self
            .windows
            .get_mut(&window)
            .ok_or(SessionError::UnknownWindow(window))?
            .primary_ui;
        state.panel_selected = Some(next);
        state.focus = PrimaryUiFocus::PanelItem(next);
        let capacity = state.layout.panel_row_capacity();
        if all_index < state.all_tabs_scroll {
            state.all_tabs_scroll = all_index;
        } else if all_index >= state.all_tabs_scroll.saturating_add(capacity) {
            state.all_tabs_scroll = all_index.saturating_add(1).saturating_sub(capacity);
        }
        let previous = previous_item.map_or(
            PrimaryUiFocus::Control(panel.anchor),
            PrimaryUiFocus::PanelItem,
        );
        let current = PrimaryUiFocus::PanelItem(next);
        self.bump_primary_ui_revision(window)?;
        Ok(PrimaryUiActionOutcome::FocusChanged { previous, current })
    }

    fn scroll_primary_panel(
        &mut self,
        window: BrowserWindowId,
        direction: PrimaryUiMoveDirection,
        rows: u8,
    ) -> Result<PrimaryUiActionOutcome, SessionError> {
        if rows == 0 || rows > MAX_PRIMARY_UI_SCROLL_ROWS {
            return Ok(PrimaryUiActionOutcome::NoChange);
        }
        let (current, maximum_start) = {
            let window_state = self
                .windows
                .get(&window)
                .ok_or(SessionError::UnknownWindow(window))?;
            if window_state.primary_ui.panel != Some(PrimaryUiPanel::AllTabs) {
                return Ok(PrimaryUiActionOutcome::NoChange);
            }
            let capacity = window_state.primary_ui.layout.panel_row_capacity();
            if capacity == 0 || window_state.tabs.len() <= capacity {
                return Ok(PrimaryUiActionOutcome::NoChange);
            }
            let maximum_start = window_state.tabs.len() - capacity;
            (
                window_state.primary_ui.all_tabs_scroll.min(maximum_start),
                maximum_start,
            )
        };
        let rows = usize::from(rows);
        let next = match direction {
            PrimaryUiMoveDirection::Forward => current.saturating_add(rows).min(maximum_start),
            PrimaryUiMoveDirection::Backward => current.saturating_sub(rows),
        };
        if next == current {
            return Ok(PrimaryUiActionOutcome::NoChange);
        }
        self.set_active_tab_focus(window, TabFocus::Content)?;
        let state = &mut self
            .windows
            .get_mut(&window)
            .ok_or(SessionError::UnknownWindow(window))?
            .primary_ui;
        state.all_tabs_scroll = next;
        state.panel_selected = None;
        state.focus = PrimaryUiFocus::Control(PrimaryUiControl::AllTabs);
        self.bump_primary_ui_revision(window)?;
        Ok(PrimaryUiActionOutcome::PanelScrolled {
            panel: PrimaryUiPanel::AllTabs,
            first_visible_row: next,
        })
    }

    fn set_primary_focus(
        &mut self,
        window: BrowserWindowId,
        focus: PrimaryUiFocus,
    ) -> Result<(), SessionError> {
        if !valid_primary_focus(focus) {
            return Err(SessionError::InvalidPrimaryUiFocus { focus });
        }
        let tab_focus = if focus == PrimaryUiFocus::AddressBar {
            TabFocus::Address
        } else {
            TabFocus::Content
        };
        self.set_active_tab_focus(window, tab_focus)?;
        self.windows
            .get_mut(&window)
            .ok_or(SessionError::UnknownWindow(window))?
            .primary_ui
            .focus = focus;
        Ok(())
    }

    fn set_active_tab_focus(
        &mut self,
        window: BrowserWindowId,
        focus: TabFocus,
    ) -> Result<(), SessionError> {
        let tab = self
            .windows
            .get(&window)
            .ok_or(SessionError::UnknownWindow(window))?
            .active;
        let state = self
            .tabs
            .get_mut(&tab)
            .ok_or(SessionError::UnknownTab(tab))?;
        state.focus = focus;
        if focus == TabFocus::Content {
            state.address.clear_preedit();
        }
        Ok(())
    }

    fn active_tab_primary_focus(
        &self,
        window: BrowserWindowId,
    ) -> Result<PrimaryUiFocus, SessionError> {
        let tab = self
            .windows
            .get(&window)
            .ok_or(SessionError::UnknownWindow(window))?
            .active;
        let focus = self
            .tabs
            .get(&tab)
            .ok_or(SessionError::UnknownTab(tab))?
            .focus;
        Ok(if focus == TabFocus::Address {
            PrimaryUiFocus::AddressBar
        } else {
            PrimaryUiFocus::Page
        })
    }

    fn window_contains_tab(
        &self,
        window: BrowserWindowId,
        tab: BrowserTabId,
    ) -> Result<bool, SessionError> {
        Ok(self
            .windows
            .get(&window)
            .ok_or(SessionError::UnknownWindow(window))?
            .tabs
            .contains(&tab))
    }
}

const fn valid_primary_focus(focus: PrimaryUiFocus) -> bool {
    !matches!(focus, PrimaryUiFocus::Control(PrimaryUiControl::AddressBar))
}

fn facts(
    enabled: bool,
    interaction: PrimaryUiInteraction,
    reload_stop_mode: Option<PrimaryReloadStopMode>,
    site_identity: Option<PrimarySiteIdentityKind>,
) -> PrimaryControlFacts {
    PrimaryControlFacts {
        availability: if enabled {
            PrimaryUiAvailability::Enabled
        } else {
            PrimaryUiAvailability::Disabled
        },
        interaction,
        reload_stop_mode,
        site_identity,
    }
}

fn primary_control_name(control: PrimaryUiControl, facts: PrimaryControlFacts) -> &'static str {
    match control {
        PrimaryUiControl::Back => "Back",
        PrimaryUiControl::Forward => "Forward",
        PrimaryUiControl::ReloadStop => match facts.reload_stop_mode {
            Some(PrimaryReloadStopMode::Stop) => "Stop",
            Some(PrimaryReloadStopMode::Reload) | None => "Reload",
        },
        PrimaryUiControl::SiteIdentity => match facts.site_identity {
            Some(PrimarySiteIdentityKind::LoopbackHttp) => "Local site information",
            Some(PrimarySiteIdentityKind::InsecureHttp) => "Not secure",
            Some(PrimarySiteIdentityKind::Unverified) => "Connection not verified",
            Some(PrimarySiteIdentityKind::NoPage) | None => "Site information",
        },
        PrimaryUiControl::AddressBar => "Address and search bar",
        PrimaryUiControl::NewTab => "New Tab",
        PrimaryUiControl::AllTabs => "List all tabs",
        PrimaryUiControl::ApplicationMenu => "Application menu",
        PrimaryUiControl::Overflow => "More tools",
    }
}

fn primary_focus_matches_control(focus: PrimaryUiFocus, control: PrimaryUiControl) -> bool {
    match control {
        PrimaryUiControl::AddressBar => focus == PrimaryUiFocus::AddressBar,
        _ => focus == PrimaryUiFocus::Control(control),
    }
}

fn primary_tab_name(tab: &TabState) -> Box<str> {
    let address = tab
        .history_index
        .and_then(|index| tab.history.get(index))
        .map(|entry| entry.address.as_ref())
        .filter(|address| !address.is_empty())
        .unwrap_or("New Tab");
    bounded_primary_label(address)
}

fn bounded_primary_label(value: &str) -> Box<str> {
    if value.len() <= MAX_PRIMARY_UI_LABEL_BYTES {
        return value.into();
    }
    let mut end = MAX_PRIMARY_UI_LABEL_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].into()
}

fn classify_site_identity(address: Option<&str>) -> PrimarySiteIdentityKind {
    let Some(address) = address else {
        return PrimarySiteIdentityKind::NoPage;
    };
    if http_authority_is_numeric_loopback(address) {
        PrimarySiteIdentityKind::LoopbackHttp
    } else if address.starts_with("http://") {
        PrimarySiteIdentityKind::InsecureHttp
    } else {
        PrimarySiteIdentityKind::Unverified
    }
}

fn http_authority_is_numeric_loopback(address: &str) -> bool {
    let Some(after_scheme) = address.strip_prefix("http://") else {
        return false;
    };
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default();
    // User information is not a host identity. Looking only at a URL prefix
    // would incorrectly label `127.0.0.1@public.example` as loopback.
    let host_and_port = authority.rsplit('@').next().unwrap_or_default();
    if let Some(bracketed) = host_and_port.strip_prefix('[') {
        let Some((host, suffix)) = bracketed.split_once(']') else {
            return false;
        };
        let port_is_valid = suffix.is_empty()
            || suffix.strip_prefix(':').is_some_and(|port| {
                !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit())
            });
        return host == "::1" && port_is_valid;
    }
    let host = match host_and_port.rsplit_once(':') {
        Some((host, port))
            if !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            host
        }
        _ => host_and_port,
    };
    host.parse::<std::net::Ipv4Addr>()
        .is_ok_and(|address| address.is_loopback())
}

fn primary_panel_item_is_enabled(
    window: &WindowState,
    panel: PrimaryUiPanel,
    item: PrimaryUiPanelItemId,
    facts: &[PrimaryControlFacts; PrimaryUiControl::ALL.len()],
) -> bool {
    let control_enabled =
        |control: PrimaryUiControl| facts[usize::from(control as u8)].availability.is_enabled();
    match (panel, item) {
        (PrimaryUiPanel::AllTabs, PrimaryUiPanelItemId::AllTabsTab(tab)) => {
            window.tabs.contains(&tab)
        }
        (PrimaryUiPanel::ApplicationMenu, PrimaryUiPanelItemId::ApplicationNewTab) => {
            control_enabled(PrimaryUiControl::NewTab)
        }
        (PrimaryUiPanel::ApplicationMenu, PrimaryUiPanelItemId::ApplicationCloseTab) => {
            !window.tabs.is_empty()
        }
        (PrimaryUiPanel::ApplicationMenu, PrimaryUiPanelItemId::ApplicationBack) => {
            control_enabled(PrimaryUiControl::Back)
        }
        (PrimaryUiPanel::ApplicationMenu, PrimaryUiPanelItemId::ApplicationForward) => {
            control_enabled(PrimaryUiControl::Forward)
        }
        (PrimaryUiPanel::ApplicationMenu, PrimaryUiPanelItemId::ApplicationReloadStop) => {
            control_enabled(PrimaryUiControl::ReloadStop)
        }
        (PrimaryUiPanel::Overflow, PrimaryUiPanelItemId::OverflowControl(control)) => {
            window.primary_ui.layout.overflowed().contains(control) && control_enabled(control)
        }
        _ => false,
    }
}

fn first_enabled_primary_panel_item(
    window: &WindowState,
    panel: PrimaryUiPanel,
    facts: &[PrimaryControlFacts; PrimaryUiControl::ALL.len()],
) -> Option<PrimaryUiPanelItemId> {
    match panel {
        PrimaryUiPanel::SiteIdentity => None,
        PrimaryUiPanel::AllTabs => window
            .tabs
            .iter()
            .copied()
            .find(|tab| *tab == window.active)
            .or_else(|| window.tabs.first().copied())
            .map(PrimaryUiPanelItemId::AllTabsTab),
        PrimaryUiPanel::ApplicationMenu => [
            PrimaryUiPanelItemId::ApplicationNewTab,
            PrimaryUiPanelItemId::ApplicationBack,
            PrimaryUiPanelItemId::ApplicationForward,
            PrimaryUiPanelItemId::ApplicationReloadStop,
            PrimaryUiPanelItemId::ApplicationCloseTab,
        ]
        .into_iter()
        .find(|item| primary_panel_item_is_enabled(window, panel, *item, facts)),
        PrimaryUiPanel::Overflow => window
            .primary_ui
            .layout
            .overflowed()
            .iter()
            .map(PrimaryUiPanelItemId::OverflowControl)
            .find(|item| primary_panel_item_is_enabled(window, panel, *item, facts)),
    }
}

fn primary_panel_item_index(
    window: &WindowState,
    panel: PrimaryUiPanel,
    item: PrimaryUiPanelItemId,
) -> Option<usize> {
    match (panel, item) {
        (PrimaryUiPanel::SiteIdentity, PrimaryUiPanelItemId::IdentitySummary) => Some(0),
        (PrimaryUiPanel::AllTabs, PrimaryUiPanelItemId::AllTabsTab(tab)) => {
            window.tabs.iter().position(|candidate| *candidate == tab)
        }
        (PrimaryUiPanel::ApplicationMenu, item) => [
            PrimaryUiPanelItemId::ApplicationNewTab,
            PrimaryUiPanelItemId::ApplicationBack,
            PrimaryUiPanelItemId::ApplicationForward,
            PrimaryUiPanelItemId::ApplicationReloadStop,
            PrimaryUiPanelItemId::ApplicationCloseTab,
        ]
        .iter()
        .position(|candidate| *candidate == item),
        (PrimaryUiPanel::Overflow, PrimaryUiPanelItemId::OverflowControl(control)) => window
            .primary_ui
            .layout
            .overflowed()
            .iter()
            .position(|candidate| candidate == control),
        _ => None,
    }
}

#[allow(clippy::too_many_lines)]
fn primary_panel_items(
    window: &WindowState,
    panel: PrimaryUiPanel,
    controls: &[PrimaryUiControlSnapshot],
    tabs: &[PrimaryUiTabSnapshot],
) -> Result<Vec<PrimaryUiPanelItemSnapshot>, SessionError> {
    let mut rows = Vec::new();
    let expected = match panel {
        PrimaryUiPanel::SiteIdentity => 1,
        PrimaryUiPanel::AllTabs => tabs.len(),
        PrimaryUiPanel::ApplicationMenu => 5,
        PrimaryUiPanel::Overflow => window.primary_ui.layout.overflowed().iter().count(),
    };
    rows.try_reserve_exact(expected)
        .map_err(|_| SessionError::PrimaryUiResource {
            detail: "could not reserve primary panel rows",
        })?;
    let focused = window.primary_ui.panel_selected;
    match panel {
        PrimaryUiPanel::SiteIdentity => {
            let identity = control_snapshot(controls, PrimaryUiControl::SiteIdentity);
            let name = match identity.site_identity {
                Some(PrimarySiteIdentityKind::LoopbackHttp) => {
                    "Local HTTP connection is not secure"
                }
                Some(PrimarySiteIdentityKind::InsecureHttp) => "HTTP connection is not secure",
                Some(PrimarySiteIdentityKind::Unverified) => "Connection security is not verified",
                Some(PrimarySiteIdentityKind::NoPage) | None => "No site information",
            };
            rows.push(panel_row(
                PrimaryUiPanelItemId::IdentitySummary,
                name,
                PrimaryUiAvailability::Disabled,
                PrimaryUiInteraction::None,
                false,
                focused,
                PrimaryUiPanelItemAction::None,
            ));
        }
        PrimaryUiPanel::AllTabs => {
            for tab in tabs {
                rows.push(PrimaryUiPanelItemSnapshot {
                    id: PrimaryUiPanelItemId::AllTabsTab(tab.tab),
                    name: tab.name.clone(),
                    availability: PrimaryUiAvailability::Enabled,
                    interaction: PrimaryUiInteraction::Invoke,
                    selected: tab.selected,
                    expanded: false,
                    focused: focused == Some(PrimaryUiPanelItemId::AllTabsTab(tab.tab)),
                    action: PrimaryUiPanelItemAction::ActivateTab(tab.tab),
                });
            }
        }
        PrimaryUiPanel::ApplicationMenu => {
            for (id, control) in [
                (
                    PrimaryUiPanelItemId::ApplicationNewTab,
                    PrimaryUiControl::NewTab,
                ),
                (
                    PrimaryUiPanelItemId::ApplicationBack,
                    PrimaryUiControl::Back,
                ),
                (
                    PrimaryUiPanelItemId::ApplicationForward,
                    PrimaryUiControl::Forward,
                ),
                (
                    PrimaryUiPanelItemId::ApplicationReloadStop,
                    PrimaryUiControl::ReloadStop,
                ),
            ] {
                let control = control_snapshot(controls, control);
                rows.push(panel_row(
                    id,
                    &control.name,
                    control.availability,
                    PrimaryUiInteraction::Invoke,
                    false,
                    focused,
                    PrimaryUiPanelItemAction::InvokeControl(control.control),
                ));
            }
            rows.push(panel_row(
                PrimaryUiPanelItemId::ApplicationCloseTab,
                "Close Tab",
                PrimaryUiAvailability::Enabled,
                PrimaryUiInteraction::Invoke,
                false,
                focused,
                PrimaryUiPanelItemAction::CloseActiveTab,
            ));
        }
        PrimaryUiPanel::Overflow => {
            for control in window.primary_ui.layout.overflowed().iter() {
                let snapshot = control_snapshot(controls, control);
                rows.push(panel_row(
                    PrimaryUiPanelItemId::OverflowControl(control),
                    &snapshot.name,
                    snapshot.availability,
                    snapshot.interaction,
                    snapshot.expanded,
                    focused,
                    PrimaryUiPanelItemAction::InvokeControl(control),
                ));
            }
        }
    }
    Ok(rows)
}

fn control_snapshot(
    controls: &[PrimaryUiControlSnapshot],
    control: PrimaryUiControl,
) -> &PrimaryUiControlSnapshot {
    controls
        .iter()
        .find(|candidate| candidate.control == control)
        .expect("primary snapshot contains every fixed control")
}

#[allow(clippy::too_many_arguments)]
fn panel_row(
    id: PrimaryUiPanelItemId,
    name: &str,
    availability: PrimaryUiAvailability,
    interaction: PrimaryUiInteraction,
    selected: bool,
    focused: Option<PrimaryUiPanelItemId>,
    action: PrimaryUiPanelItemAction,
) -> PrimaryUiPanelItemSnapshot {
    PrimaryUiPanelItemSnapshot {
        id,
        name: bounded_primary_label(name),
        availability,
        interaction,
        selected,
        expanded: false,
        focused: focused == Some(id),
        action,
    }
}

fn primary_semantic_nodes(
    window: &WindowState,
    controls: &[PrimaryUiControlSnapshot],
    tabs: &[PrimaryUiTabSnapshot],
    panel: Option<&PrimaryUiPanelSnapshot>,
) -> Result<Box<[PrimaryUiSemanticNode]>, SessionError> {
    let panel_count = panel.map_or(0, |panel| panel.items.len());
    let capacity = 1_usize
        .saturating_add(tabs.len().saturating_mul(2))
        .saturating_add(controls.len())
        .saturating_add(panel_count);
    let mut semantics = Vec::new();
    semantics
        .try_reserve_exact(capacity)
        .map_err(|_| SessionError::PrimaryUiResource {
            detail: "could not reserve primary semantic nodes",
        })?;
    semantics.push(PrimaryUiSemanticNode {
        id: PrimaryUiElementId::Page,
        role: PrimaryUiRole::Document,
        name: "Page content".into(),
        enabled: true,
        selected: false,
        expanded: false,
        focused: window.primary_ui.focus == PrimaryUiFocus::Page,
        visible: true,
    });
    for tab in tabs {
        semantics.push(PrimaryUiSemanticNode {
            id: PrimaryUiElementId::Tab(tab.tab),
            role: PrimaryUiRole::Tab,
            name: tab.name.clone(),
            enabled: true,
            selected: tab.selected,
            expanded: false,
            focused: tab.focused,
            visible: true,
        });
        semantics.push(PrimaryUiSemanticNode {
            id: PrimaryUiElementId::TabClose(tab.tab),
            role: PrimaryUiRole::Button,
            name: bounded_primary_label(&format!("Close {}", tab.name)),
            enabled: tab.close_availability.is_enabled(),
            selected: false,
            expanded: false,
            focused: false,
            visible: true,
        });
    }
    for control in controls.iter().filter(|control| control.visible) {
        semantics.push(PrimaryUiSemanticNode {
            id: PrimaryUiElementId::Control(control.control),
            role: if control.control == PrimaryUiControl::AddressBar {
                PrimaryUiRole::TextField
            } else {
                PrimaryUiRole::Button
            },
            name: control.name.clone(),
            enabled: control.availability.is_enabled(),
            selected: false,
            expanded: control.expanded,
            focused: control.focused,
            visible: true,
        });
    }
    if let Some(panel) = panel {
        let visible_end = panel
            .scroll_offset
            .saturating_add(panel.visible_capacity)
            .min(panel.items.len());
        for (index, item) in panel.items.iter().enumerate() {
            semantics.push(PrimaryUiSemanticNode {
                id: PrimaryUiElementId::PanelItem(item.id),
                role: PrimaryUiRole::MenuItem,
                name: item.name.clone(),
                enabled: item.availability.is_enabled(),
                selected: item.selected,
                expanded: item.expanded,
                focused: item.focused,
                visible: index >= panel.scroll_offset && index < visible_end,
            });
        }
    }
    Ok(semantics.into_boxed_slice())
}

fn primary_focus_candidates(
    snapshot: &PrimaryUiSnapshot,
    toolbar_only: bool,
) -> Result<Vec<PrimaryUiFocus>, SessionError> {
    let active =
        snapshot
            .tabs
            .iter()
            .find(|tab| tab.selected)
            .ok_or(SessionError::PrimaryUiResource {
                detail: "primary snapshot has no selected tab",
            })?;
    let mut candidates = Vec::new();
    candidates
        .try_reserve_exact(PrimaryUiControl::ALL.len() + 2)
        .map_err(|_| SessionError::PrimaryUiResource {
            detail: "could not reserve primary focus order",
        })?;
    if !toolbar_only {
        candidates.push(PrimaryUiFocus::Tab(active.tab));
        for control in [PrimaryUiControl::NewTab, PrimaryUiControl::AllTabs] {
            push_focusable_control(&mut candidates, snapshot, control);
        }
    }
    for control in [
        PrimaryUiControl::Back,
        PrimaryUiControl::Forward,
        PrimaryUiControl::ReloadStop,
        PrimaryUiControl::SiteIdentity,
    ] {
        push_focusable_control(&mut candidates, snapshot, control);
    }
    let address = control_snapshot(&snapshot.controls, PrimaryUiControl::AddressBar);
    if address.visible && address.availability.is_enabled() && !toolbar_only {
        candidates.push(PrimaryUiFocus::AddressBar);
    }
    for control in [
        PrimaryUiControl::Overflow,
        PrimaryUiControl::ApplicationMenu,
    ] {
        push_focusable_control(&mut candidates, snapshot, control);
    }
    if !toolbar_only {
        candidates.push(PrimaryUiFocus::Page);
    }
    Ok(candidates)
}

fn push_focusable_control(
    candidates: &mut Vec<PrimaryUiFocus>,
    snapshot: &PrimaryUiSnapshot,
    control: PrimaryUiControl,
) {
    let control = control_snapshot(&snapshot.controls, control);
    if control.visible && control.availability.is_enabled() {
        candidates.push(PrimaryUiFocus::Control(control.control));
    }
}

const fn reverse_direction(direction: PrimaryUiMoveDirection) -> PrimaryUiMoveDirection {
    match direction {
        PrimaryUiMoveDirection::Forward => PrimaryUiMoveDirection::Backward,
        PrimaryUiMoveDirection::Backward => PrimaryUiMoveDirection::Forward,
    }
}

#[cfg(test)]
mod identity_tests {
    use super::{PrimarySiteIdentityKind, classify_site_identity};

    #[test]
    fn loopback_identity_parses_the_exact_authority_without_prefix_spoofing() {
        for address in [
            "http://127.0.0.1/",
            "http://127.9.8.7:8080/path",
            "http://[::1]/",
            "http://[::1]:8080/path",
        ] {
            assert_eq!(
                classify_site_identity(Some(address)),
                PrimarySiteIdentityKind::LoopbackHttp,
                "{address}",
            );
        }
        for address in [
            "http://127.example/",
            "http://127.0.0.1@public.example/",
            "http://[::1].public.example/",
            "http://[::1]:not-a-port/",
        ] {
            assert_eq!(
                classify_site_identity(Some(address)),
                PrimarySiteIdentityKind::InsecureHttp,
                "{address}",
            );
        }
        assert_eq!(
            classify_site_identity(Some("https://127.0.0.1/")),
            PrimarySiteIdentityKind::Unverified,
            "an HTTPS spelling cannot invent transport verification",
        );
    }
}
