use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

use super::*;
use crate::{EnginePortExecutorShutdown, EnginePortSequence, PrimaryUiControlSet};

#[derive(Default)]
struct TestPortState {
    generations: BTreeMap<TopLevelContextId, NavigationGeneration>,
    events: VecDeque<EnginePortEvent>,
    next_event_sequence: u64,
    navigations: usize,
    cancellations: Vec<NavigationId>,
}

#[derive(Clone)]
struct TestPortHandle(Rc<RefCell<TestPortState>>);

impl TestPortHandle {
    fn push(&self, kind: EnginePortEventKind) {
        let mut state = self.0.borrow_mut();
        if state.next_event_sequence == 0 {
            state.next_event_sequence = 1;
        }
        let sequence = EnginePortSequence::new(state.next_event_sequence).unwrap();
        state.next_event_sequence += 1;
        state.events.push_back(EnginePortEvent::new(sequence, kind));
    }
}

struct TestPort(Rc<RefCell<TestPortState>>);

impl TestPort {
    fn pair() -> (Self, TestPortHandle) {
        let state = Rc::new(RefCell::new(TestPortState::default()));
        (Self(Rc::clone(&state)), TestPortHandle(state))
    }
}

impl EnginePort for TestPort {
    fn navigate(
        &mut self,
        context: TopLevelContextId,
        _request: NavigationRequest,
    ) -> Result<NavigationId, EnginePortError> {
        let mut state = self.0.borrow_mut();
        let generation = state
            .generations
            .get(&context)
            .copied()
            .map_or(
                Some(NavigationGeneration::INITIAL),
                NavigationGeneration::checked_next,
            )
            .ok_or(EnginePortError::Command(
                CommandErrorKind::GenerationExhausted,
            ))?;
        state.generations.insert(context, generation);
        state.navigations += 1;
        Ok(NavigationId::new(context, generation))
    }

    fn cancel_navigation(&mut self, navigation: NavigationId) -> Result<(), EnginePortError> {
        self.0.borrow_mut().cancellations.push(navigation);
        Ok(())
    }

    fn close_context(&mut self, _navigation: NavigationId) -> Result<(), EnginePortError> {
        Ok(())
    }

    fn poll_event(&mut self) -> Result<Option<EnginePortEvent>, EnginePortError> {
        Ok(self.0.borrow_mut().events.pop_front())
    }

    fn take_frame(
        &mut self,
        _navigation: NavigationId,
        _lease: EnginePortFrameLeaseId,
    ) -> Result<EngineFrameLease, EnginePortError> {
        Err(EnginePortError::FrameLease(FrameLeaseError::Unknown))
    }

    fn take_mutation_result(
        &mut self,
        _navigation: NavigationId,
        _lease: EnginePortMutationLeaseId,
    ) -> Result<EngineMutationResultLease, EnginePortError> {
        Err(EnginePortError::MutationLease(
            MutationResultLeaseError::Unknown,
        ))
    }

    fn shutdown(&mut self) -> EnginePortShutdownStatus {
        EnginePortShutdownStatus::new(
            EnginePortStopReason::Requested,
            EnginePortExecutorShutdown::Clean,
        )
    }
}

fn limits() -> SessionLimits {
    SessionLimits::new(1, 64, 64, 64, 64, 64 * 1024, 64 * 1024, 16 * 1024, 64).unwrap()
}

fn session() -> (BrowserSession<TestPort>, TestPortHandle) {
    let (port, handle) = TestPort::pair();
    (BrowserSession::new(port, limits()).unwrap(), handle)
}

fn ids() -> (BrowserWindowId, BrowserTabId) {
    (
        BrowserWindowId::new(1).unwrap(),
        BrowserTabId::new(1).unwrap(),
    )
}

fn dispatch(
    session: &mut BrowserSession<TestPort>,
    window: BrowserWindowId,
    action: PrimaryUiAction,
) -> PrimaryUiActionOutcome {
    let revision = session.primary_ui_revision(window).unwrap();
    session
        .dispatch_primary_ui_action(window, revision, action)
        .unwrap()
}

fn control(snapshot: &PrimaryUiSnapshot, target: PrimaryUiControl) -> &PrimaryUiControlSnapshot {
    snapshot
        .controls
        .iter()
        .find(|control| control.control == target)
        .unwrap()
}

#[test]
fn stale_and_disabled_actions_have_no_engine_or_product_effect() {
    let (mut session, handle) = session();
    let (window, tab) = ids();
    let initial = session.primary_ui_snapshot(window).unwrap();
    assert_eq!(
        dispatch(
            &mut session,
            window,
            PrimaryUiAction::InvokeControl(PrimaryUiControl::Back),
        ),
        PrimaryUiActionOutcome::Disabled(PrimaryUiElementId::Control(PrimaryUiControl::Back))
    );
    assert_eq!(handle.0.borrow().navigations, 0);
    assert_eq!(session.tab_count(), 1);

    let binding = initial
        .bind_action(PrimaryUiElementId::Control(PrimaryUiControl::NewTab))
        .unwrap();
    session.focus_content(tab).unwrap();
    assert!(matches!(
        session.dispatch_primary_ui_binding(binding).unwrap(),
        PrimaryUiActionOutcome::Stale { .. }
    ));
    assert_eq!(session.tab_count(), 1);
    assert_eq!(handle.0.borrow().navigations, 0);
}

#[test]
fn reload_stop_mode_and_availability_follow_exact_loading_state() {
    let (mut session, handle) = session();
    let (window, tab) = ids();
    let navigation = match session.navigate_new(tab, "http://example.test/").unwrap() {
        BrowserCommandOutcome::NavigationQueued { navigation, .. } => navigation,
        outcome => panic!("unexpected outcome: {outcome:?}"),
    };
    let loading = session.primary_ui_snapshot(window).unwrap();
    let reload_stop = control(&loading, PrimaryUiControl::ReloadStop);
    assert_eq!(
        reload_stop.reload_stop_mode,
        Some(PrimaryReloadStopMode::Stop)
    );
    assert_eq!(reload_stop.availability, PrimaryUiAvailability::Enabled);

    session.stop(tab).unwrap();
    let stopping = session.primary_ui_snapshot(window).unwrap();
    let reload_stop = control(&stopping, PrimaryUiControl::ReloadStop);
    assert_eq!(
        reload_stop.reload_stop_mode,
        Some(PrimaryReloadStopMode::Stop)
    );
    assert_eq!(reload_stop.availability, PrimaryUiAvailability::Disabled);
    assert_eq!(handle.0.borrow().cancellations.as_slice(), [navigation]);

    handle.push(EnginePortEventKind::NavigationCancelled { navigation });
    assert_eq!(
        session.poll_engine_once().unwrap(),
        EnginePumpOutcome::Applied
    );
    let cancelled = session.primary_ui_snapshot(window).unwrap();
    let reload_stop = control(&cancelled, PrimaryUiControl::ReloadStop);
    assert_eq!(
        reload_stop.reload_stop_mode,
        Some(PrimaryReloadStopMode::Reload)
    );
    assert_eq!(reload_stop.availability, PrimaryUiAvailability::Enabled);
}

#[test]
fn all_tabs_popup_retains_complete_inventory_and_exact_scroll_window() {
    let (mut session, _handle) = session();
    let (window, _) = ids();
    for _ in 1..20 {
        session.open_tab(window).unwrap();
    }
    let layout = PrimaryUiLayout::new(
        PrimaryUiControlSet::wide_defaults(),
        PrimaryUiControlSet::empty(),
        3,
    )
    .unwrap();
    session.set_primary_ui_layout(window, layout).unwrap();
    assert_eq!(
        dispatch(
            &mut session,
            window,
            PrimaryUiAction::InvokeControl(PrimaryUiControl::AllTabs),
        ),
        PrimaryUiActionOutcome::PanelChanged(Some(PrimaryUiPanel::AllTabs))
    );
    let snapshot = session.primary_ui_snapshot(window).unwrap();
    let panel = snapshot.panel.as_ref().unwrap();
    assert_eq!(panel.items.len(), 20);
    assert_eq!(panel.total_rows, 20);
    assert_eq!(panel.visible_capacity, 3);
    assert_eq!(panel.scroll_offset, 17);
    assert_eq!(
        snapshot
            .semantics
            .iter()
            .filter(|node| matches!(node.id, PrimaryUiElementId::PanelItem(_)) && node.visible)
            .count(),
        3
    );

    let scroll = snapshot
        .bind_panel_scroll(PrimaryUiMoveDirection::Backward, 4)
        .unwrap();
    assert_eq!(
        session.dispatch_primary_ui_binding(scroll).unwrap(),
        PrimaryUiActionOutcome::PanelScrolled {
            panel: PrimaryUiPanel::AllTabs,
            first_visible_row: 13,
        },
    );
    let scrolled = session.primary_ui_snapshot(window).unwrap();
    let panel = scrolled.panel.as_ref().unwrap();
    assert_eq!(panel.scroll_offset, 13);
    assert_eq!(panel.selected, None);
    assert_eq!(
        scrolled.focus,
        PrimaryUiFocus::Control(PrimaryUiControl::AllTabs)
    );
    assert_eq!(
        scrolled
            .semantics
            .iter()
            .filter(|node| matches!(node.id, PrimaryUiElementId::PanelItem(_)) && node.visible)
            .count(),
        3,
    );
    assert!(matches!(
        session.dispatch_primary_ui_binding(scroll).unwrap(),
        PrimaryUiActionOutcome::Stale { .. }
    ));

    let to_end = scrolled
        .bind_panel_scroll(PrimaryUiMoveDirection::Forward, MAX_PRIMARY_UI_SCROLL_ROWS)
        .unwrap();
    assert_eq!(
        session.dispatch_primary_ui_binding(to_end).unwrap(),
        PrimaryUiActionOutcome::PanelScrolled {
            panel: PrimaryUiPanel::AllTabs,
            first_visible_row: 17,
        },
    );
}

#[test]
fn narrow_layout_relocates_new_tab_without_losing_identity_or_action() {
    let (mut session, _handle) = session();
    let (window, _) = ids();
    let mut visible = PrimaryUiControlSet::empty();
    for control in PrimaryUiControl::ALL {
        if control != PrimaryUiControl::NewTab {
            visible = visible.with(control);
        }
    }
    let overflowed = PrimaryUiControlSet::empty().with(PrimaryUiControl::NewTab);
    let layout = PrimaryUiLayout::new(visible, overflowed, 4).unwrap();
    session.set_primary_ui_layout(window, layout).unwrap();
    let snapshot = session.primary_ui_snapshot(window).unwrap();
    let new_tab = control(&snapshot, PrimaryUiControl::NewTab);
    assert!(!new_tab.visible);
    assert!(new_tab.overflowed);
    assert!(control(&snapshot, PrimaryUiControl::Overflow).visible);

    dispatch(
        &mut session,
        window,
        PrimaryUiAction::InvokeControl(PrimaryUiControl::Overflow),
    );
    let snapshot = session.primary_ui_snapshot(window).unwrap();
    let panel = snapshot.panel.as_ref().unwrap();
    assert_eq!(panel.panel, PrimaryUiPanel::Overflow);
    assert_eq!(panel.items.len(), 1);
    assert_eq!(
        panel.items[0].id,
        PrimaryUiPanelItemId::OverflowControl(PrimaryUiControl::NewTab)
    );
    let binding = snapshot
        .bind_action(PrimaryUiElementId::PanelItem(panel.items[0].id))
        .unwrap();
    assert_eq!(binding.window(), window);
    assert_eq!(binding.revision(), snapshot.revision);
}

#[test]
fn ltr_and_rtl_toolbar_traversal_skip_disabled_controls() {
    let (mut session, _handle) = session();
    let (window, tab) = ids();
    session.navigate_new(tab, "http://one.test/").unwrap();
    session.navigate_new(tab, "http://two.test/").unwrap();
    assert!(matches!(
        dispatch(
            &mut session,
            window,
            PrimaryUiAction::MoveFocus(PrimaryUiMoveDirection::Backward),
        ),
        PrimaryUiActionOutcome::FocusChanged {
            current: PrimaryUiFocus::Control(PrimaryUiControl::ReloadStop),
            ..
        }
    ));
    assert!(matches!(
        dispatch(
            &mut session,
            window,
            PrimaryUiAction::MoveToolbarFocus(PrimaryUiMoveDirection::Backward),
        ),
        PrimaryUiActionOutcome::FocusChanged {
            current: PrimaryUiFocus::Control(PrimaryUiControl::Back),
            ..
        }
    ));
    session
        .set_primary_ui_direction(window, PrimaryUiDirection::RightToLeft)
        .unwrap();
    assert!(matches!(
        dispatch(
            &mut session,
            window,
            PrimaryUiAction::MoveToolbarFocus(PrimaryUiMoveDirection::Backward),
        ),
        PrimaryUiActionOutcome::FocusChanged {
            current: PrimaryUiFocus::Control(PrimaryUiControl::ReloadStop),
            ..
        }
    ));
}

#[test]
fn escape_dismisses_popup_then_restores_page_focus() {
    let (mut session, _handle) = session();
    let (window, _) = ids();
    dispatch(
        &mut session,
        window,
        PrimaryUiAction::InvokeControl(PrimaryUiControl::ApplicationMenu),
    );
    assert!(session.primary_ui_snapshot(window).unwrap().panel.is_some());
    assert_eq!(
        dispatch(&mut session, window, PrimaryUiAction::DismissPanel),
        PrimaryUiActionOutcome::PanelChanged(None)
    );
    assert_eq!(
        session.primary_ui_snapshot(window).unwrap().focus,
        PrimaryUiFocus::Control(PrimaryUiControl::ApplicationMenu)
    );
    assert!(matches!(
        dispatch(&mut session, window, PrimaryUiAction::DismissPanel),
        PrimaryUiActionOutcome::FocusChanged {
            current: PrimaryUiFocus::Page,
            ..
        }
    ));
}

#[test]
fn opening_popup_from_address_focus_disables_hidden_editor_and_clears_preedit() {
    let (mut session, _handle) = session();
    let (window, tab) = ids();
    session
        .tabs
        .get_mut(&tab)
        .unwrap()
        .address
        .set_preedit("compose", Some(1..4))
        .unwrap();
    assert!(session.tab_snapshot(tab).unwrap().address_focused);
    assert!(session.tabs.get(&tab).unwrap().address.preedit().is_some());

    assert_eq!(
        dispatch(
            &mut session,
            window,
            PrimaryUiAction::InvokeControl(PrimaryUiControl::ApplicationMenu),
        ),
        PrimaryUiActionOutcome::PanelChanged(Some(PrimaryUiPanel::ApplicationMenu)),
    );
    assert!(!session.tab_snapshot(tab).unwrap().address_focused);
    assert!(session.tabs.get(&tab).unwrap().address.preedit().is_none());
    assert!(matches!(
        session.primary_ui_snapshot(window).unwrap().focus,
        PrimaryUiFocus::PanelItem(_)
    ));

    assert_eq!(
        dispatch(&mut session, window, PrimaryUiAction::DismissPanel),
        PrimaryUiActionOutcome::PanelChanged(None),
    );
    assert_eq!(
        session.primary_ui_snapshot(window).unwrap().focus,
        PrimaryUiFocus::Control(PrimaryUiControl::ApplicationMenu),
    );
    assert!(!session.tab_snapshot(tab).unwrap().address_focused);
}

#[test]
fn zero_capacity_resize_closes_popup_to_page_content_without_split_focus() {
    let (mut session, _handle) = session();
    let (window, tab) = ids();
    dispatch(
        &mut session,
        window,
        PrimaryUiAction::InvokeControl(PrimaryUiControl::ApplicationMenu),
    );
    assert!(session.primary_ui_snapshot(window).unwrap().panel.is_some());
    assert!(!session.tab_snapshot(tab).unwrap().address_focused);

    session
        .set_primary_ui_layout(
            window,
            PrimaryUiLayout::new(
                PrimaryUiControlSet::wide_defaults(),
                PrimaryUiControlSet::empty(),
                0,
            )
            .unwrap(),
        )
        .unwrap();
    let resized = session.primary_ui_snapshot(window).unwrap();
    assert_eq!(resized.panel, None);
    assert_eq!(resized.focus, PrimaryUiFocus::Page);
    assert!(!session.tab_snapshot(tab).unwrap().address_focused);
    assert!(
        resized
            .semantics
            .iter()
            .any(|node| node.id == PrimaryUiElementId::Page && node.focused)
    );
}

#[test]
fn focused_panel_row_is_repaired_before_new_state_can_disable_it() {
    let (mut session, _handle) = session();
    let (window, tab) = ids();
    session.navigate_new(tab, "http://one.test/").unwrap();
    session.navigate_new(tab, "http://two.test/").unwrap();
    dispatch(
        &mut session,
        window,
        PrimaryUiAction::InvokeControl(PrimaryUiControl::ApplicationMenu),
    );
    assert!(matches!(
        dispatch(
            &mut session,
            window,
            PrimaryUiAction::MovePanelSelection(PrimaryUiMoveDirection::Forward),
        ),
        PrimaryUiActionOutcome::FocusChanged {
            current: PrimaryUiFocus::PanelItem(PrimaryUiPanelItemId::ApplicationBack),
            ..
        }
    ));

    session.go_history(tab, -1).unwrap();
    let repaired = session.primary_ui_snapshot(window).unwrap();
    assert_eq!(
        repaired.panel.as_ref().unwrap().panel,
        PrimaryUiPanel::ApplicationMenu,
    );
    assert_ne!(
        repaired.focus,
        PrimaryUiFocus::PanelItem(PrimaryUiPanelItemId::ApplicationBack),
    );
    let focused = repaired
        .semantics
        .iter()
        .find(|node| node.focused)
        .expect("repaired primary UI retains one focus owner");
    assert!(focused.enabled, "projection cannot freeze disabled focus");
}

#[test]
fn snapshot_bindings_are_exact_for_controls_tabs_closes_and_panel_rows() {
    let (mut session, _handle) = session();
    let (window, first) = ids();
    let second = match session.open_tab(window).unwrap() {
        BrowserCommandOutcome::TabOpened { tab, .. } => tab,
        outcome => panic!("unexpected outcome: {outcome:?}"),
    };
    let snapshot = session.primary_ui_snapshot(window).unwrap();
    let activate_first = snapshot
        .bind_action(PrimaryUiElementId::Tab(first))
        .unwrap();
    assert_eq!(activate_first.action(), PrimaryUiAction::ActivateTab(first));
    assert!(matches!(
        session
            .dispatch_primary_ui_binding(activate_first)
            .unwrap(),
        PrimaryUiActionOutcome::Command(BrowserCommandOutcome::TabActivated { tab, .. })
            if tab == first
    ));

    let snapshot = session.primary_ui_snapshot(window).unwrap();
    let close_second = snapshot
        .bind_action(PrimaryUiElementId::TabClose(second))
        .unwrap();
    assert_eq!(close_second.action(), PrimaryUiAction::CloseTab(second));
    assert!(matches!(
        session.dispatch_primary_ui_binding(close_second).unwrap(),
        PrimaryUiActionOutcome::Command(BrowserCommandOutcome::TabClosed { tab, .. })
            if tab == second
    ));

    dispatch(
        &mut session,
        window,
        PrimaryUiAction::InvokeControl(PrimaryUiControl::SiteIdentity),
    );
    // No live page means identity is disabled, so no informational popup was fabricated.
    assert!(session.primary_ui_snapshot(window).unwrap().panel.is_none());
}

#[test]
fn exact_page_binding_focuses_content_once_and_stale_replay_is_suppressed() {
    let (mut session, _handle) = session();
    let (window, tab) = ids();
    let snapshot = session.primary_ui_snapshot(window).unwrap();
    assert_eq!(snapshot.focus, PrimaryUiFocus::AddressBar);
    let page = snapshot.bind_action(PrimaryUiElementId::Page).unwrap();
    assert_eq!(page.action(), PrimaryUiAction::FocusPage);

    assert_eq!(
        session.dispatch_primary_ui_binding(page).unwrap(),
        PrimaryUiActionOutcome::Command(BrowserCommandOutcome::ContentFocused { window, tab }),
    );
    let focused = session.primary_ui_snapshot(window).unwrap();
    assert_eq!(focused.focus, PrimaryUiFocus::Page);
    assert!(focused.revision > snapshot.revision);

    assert_eq!(
        session.dispatch_primary_ui_binding(page).unwrap(),
        PrimaryUiActionOutcome::Stale {
            expected: snapshot.revision,
            current: focused.revision,
        },
        "one receipt-bound page action cannot be replayed",
    );
}

#[test]
fn revision_exhaustion_is_typed_terminal_and_fail_closed() {
    let (mut session, _handle) = session();
    let (window, tab) = ids();
    session
        .windows
        .get_mut(&window)
        .unwrap()
        .primary_ui
        .revision = PrimaryUiRevision::new(u64::MAX).unwrap();
    assert_eq!(
        session.focus_content(tab).unwrap_err(),
        SessionError::Terminal(SessionFailure::PrimaryUiRevisionExhausted { window })
    );
    assert!(matches!(
        session.lifecycle(),
        SessionLifecycle::Failed {
            failure: SessionFailure::PrimaryUiRevisionExhausted { window: failed },
            ..
        } if failed == window
    ));
    assert_eq!(session.window_count(), 0);
    assert_eq!(session.tab_count(), 0);
}
