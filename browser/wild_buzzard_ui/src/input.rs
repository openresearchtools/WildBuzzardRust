use wild_buzzard_platform::{InputEvent, KeyEvent, KeyState, Modifiers};

use crate::CursorMove;

const KEY_ESCAPE: u32 = 1;
const KEY_DIGIT_1: u32 = 2;
const KEY_DIGIT_8: u32 = 9;
const KEY_DIGIT_9: u32 = 10;
const KEY_BACKSPACE: u32 = 14;
const KEY_TAB: u32 = 15;
const KEY_R: u32 = 19;
const KEY_T: u32 = 20;
const KEY_ENTER: u32 = 28;
const KEY_A: u32 = 30;
const KEY_L: u32 = 38;
const KEY_W: u32 = 17;
const KEY_D: u32 = 32;
const KEY_F4: u32 = 62;
const KEY_F5: u32 = 63;
const KEY_HOME: u32 = 102;
const KEY_PAGE_UP: u32 = 104;
const KEY_LEFT: u32 = 105;
const KEY_RIGHT: u32 = 106;
const KEY_END: u32 = 107;
const KEY_PAGE_DOWN: u32 = 109;
const KEY_DELETE: u32 = 111;

const ACTIVE_MODIFIERS: u16 = Modifiers::SHIFT.bits()
    | Modifiers::CONTROL.bits()
    | Modifiers::ALT.bits()
    | Modifiers::SUPER.bits();

/// Browser-level shortcut recognized from Linux physical-key events.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LinuxShortcut {
    NewTab,
    CloseTab,
    FocusAddress,
    Back,
    Forward,
    Reload,
    Stop,
    NextTab,
    PreviousTab,
    /// Select positions one through eight; nine selects the last tab.
    ActivatePosition {
        one_based: u8,
    },
}

/// Address edit or browser shortcut mapped from one platform-neutral event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LinuxInputAction {
    Shortcut(LinuxShortcut),
    InsertText(Box<str>),
    SelectAll,
    Backspace,
    DeleteForward,
    MoveCursor {
        movement: CursorMove,
        extend: bool,
    },
    SubmitAddress,
    /// Address focus gives this revert-before-stop semantics.
    Escape,
}

/// Maps the exact Linux physical-scancode contract exposed by the window shell.
///
/// Text comes only from `InputEvent::Text`; physical key events never invent a
/// layout-dependent character. Lock modifiers are intentionally ignored while
/// matching shortcuts. Key-up events and unrelated content input return `None`.
#[must_use]
pub fn map_linux_input(event: &InputEvent, address_focused: bool) -> Option<LinuxInputAction> {
    match event {
        InputEvent::Text(text) if address_focused => Some(LinuxInputAction::InsertText(
            text.text().to_owned().into_boxed_str(),
        )),
        InputEvent::Key(key) if key.state == KeyState::Down => map_key(*key, address_focused),
        InputEvent::Pointer(_)
        | InputEvent::Scroll(_)
        | InputEvent::Text(_)
        | InputEvent::Key(_) => None,
    }
}

fn map_key(key: KeyEvent, address_focused: bool) -> Option<LinuxInputAction> {
    let physical = key.physical_key.0;
    let modifiers = key.metadata.modifiers.bits() & ACTIVE_MODIFIERS;
    let shift = Modifiers::SHIFT.bits();
    let control = Modifiers::CONTROL.bits();
    let alt = Modifiers::ALT.bits();

    let shortcut = match (physical, modifiers) {
        (KEY_T, value) if value == control => Some(LinuxShortcut::NewTab),
        (KEY_W | KEY_F4, value) if value == control => Some(LinuxShortcut::CloseTab),
        (KEY_L, value) if value == control => Some(LinuxShortcut::FocusAddress),
        (KEY_D, value) if value == alt => Some(LinuxShortcut::FocusAddress),
        (KEY_LEFT, value) if value == alt => Some(LinuxShortcut::Back),
        (KEY_RIGHT, value) if value == alt => Some(LinuxShortcut::Forward),
        (KEY_R, value) if value == control => Some(LinuxShortcut::Reload),
        (KEY_F5, 0) => Some(LinuxShortcut::Reload),
        (KEY_ESCAPE, 0) if !address_focused => Some(LinuxShortcut::Stop),
        (KEY_TAB | KEY_PAGE_DOWN, value) if value == control => Some(LinuxShortcut::NextTab),
        (KEY_TAB, value) if value == (control | shift) => Some(LinuxShortcut::PreviousTab),
        (KEY_PAGE_UP, value) if value == control => Some(LinuxShortcut::PreviousTab),
        (KEY_DIGIT_1..=KEY_DIGIT_8, value) if value == alt => {
            let one_based = u8::try_from(physical - KEY_DIGIT_1 + 1).ok()?;
            Some(LinuxShortcut::ActivatePosition { one_based })
        }
        (KEY_DIGIT_9, value) if value == alt => {
            Some(LinuxShortcut::ActivatePosition { one_based: 9 })
        }
        _ => None,
    };
    if let Some(shortcut) = shortcut {
        return Some(LinuxInputAction::Shortcut(shortcut));
    }
    if !address_focused {
        return None;
    }

    match (physical, modifiers) {
        (KEY_A, value) if value == control => Some(LinuxInputAction::SelectAll),
        (KEY_ENTER, 0) => Some(LinuxInputAction::SubmitAddress),
        (KEY_ESCAPE, 0) => Some(LinuxInputAction::Escape),
        (KEY_BACKSPACE, 0) => Some(LinuxInputAction::Backspace),
        (KEY_DELETE, 0) => Some(LinuxInputAction::DeleteForward),
        (KEY_LEFT, value) if value == 0 || value == shift => Some(LinuxInputAction::MoveCursor {
            movement: CursorMove::Previous,
            extend: modifiers == shift,
        }),
        (KEY_RIGHT, value) if value == 0 || value == shift => Some(LinuxInputAction::MoveCursor {
            movement: CursorMove::Next,
            extend: modifiers == shift,
        }),
        (KEY_HOME, value) if value == 0 || value == shift => Some(LinuxInputAction::MoveCursor {
            movement: CursorMove::Start,
            extend: modifiers == shift,
        }),
        (KEY_END, value) if value == 0 || value == shift => Some(LinuxInputAction::MoveCursor {
            movement: CursorMove::End,
            extend: modifiers == shift,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use wild_buzzard_platform::{
        EventSequence, EventTimestampMicros, InputDeviceId, InputEvent, InputMetadata, KeyEvent,
        KeyLocation, KeyState, Modifiers, PhysicalKeyCode, SeatId, SurfaceIdAllocator,
        SurfaceNamespace,
    };

    use super::{KEY_LEFT, KEY_PAGE_UP, KEY_TAB, LinuxInputAction, LinuxShortcut, map_linux_input};

    fn key(scancode: u32, modifiers: Modifiers) -> InputEvent {
        let namespace = SurfaceNamespace::new(1).unwrap();
        let mut surfaces = SurfaceIdAllocator::new(namespace);
        let surface = surfaces.allocate().unwrap();
        InputEvent::Key(KeyEvent {
            metadata: InputMetadata {
                sequence: EventSequence::new(1).unwrap(),
                timestamp: EventTimestampMicros(1),
                seat: SeatId::new(1).unwrap(),
                device: InputDeviceId::new(1).unwrap(),
                surface,
                modifiers,
            },
            physical_key: PhysicalKeyCode(scancode),
            state: KeyState::Down,
            location: KeyLocation::Standard,
            repeat: false,
        })
    }

    #[test]
    fn firefox_linux_tab_shortcuts_are_exact() {
        assert_eq!(
            map_linux_input(
                &key(KEY_TAB, Modifiers::CONTROL.union(Modifiers::SHIFT)),
                false,
            ),
            Some(LinuxInputAction::Shortcut(LinuxShortcut::PreviousTab))
        );
        assert_eq!(
            map_linux_input(&key(KEY_PAGE_UP, Modifiers::CONTROL), false),
            Some(LinuxInputAction::Shortcut(LinuxShortcut::PreviousTab))
        );
        assert_eq!(
            map_linux_input(
                &key(KEY_PAGE_UP, Modifiers::CONTROL.union(Modifiers::SHIFT)),
                false
            ),
            None
        );
    }

    #[test]
    fn shift_arrow_is_directional_only_in_address_focus() {
        let event = key(KEY_LEFT, Modifiers::SHIFT);
        assert_eq!(
            map_linux_input(&event, true),
            Some(LinuxInputAction::MoveCursor {
                movement: crate::CursorMove::Previous,
                extend: true,
            })
        );
        assert_eq!(map_linux_input(&event, false), None);
    }
}
