use std::collections::{HashMap, HashSet};
use std::time::Instant;

use wild_buzzard_platform::{
    EventSequence, EventTimestampMicros, InputDeviceId, InputEvent, InputMetadata, KeyEvent,
    KeyLocation, KeyState, LogicalPoint, Modifiers, PhysicalKeyCode, PointerEvent, PointerId,
    PointerKind, PointerPhase, ScaleFactor, ScrollDelta, ScrollEvent, ScrollPhase, ScrollVector,
    SeatId, SurfaceId, TextInputEvent, UnitInterval,
};
use winit::dpi::{PhysicalPosition as WinitPhysicalPosition, PhysicalSize as WinitPhysicalSize};
use winit::event::{
    DeviceId, ElementState, Force, MouseButton, MouseScrollDelta, Touch, TouchPhase,
};
use winit::keyboard::{
    KeyLocation as WinitKeyLocation, ModifiersState, NativeKeyCode, PhysicalKey,
};
use winit::platform::scancode::PhysicalKeyExtScancode;

use crate::config::LinuxShellLimits;
use crate::event::{BoundedImeText, InputOrigin, LinuxStopReason};

const SEAT_ID: u64 = 1;
// Winit's IME event has no DeviceId. Reserve an explicit adapter-owned virtual
// identity rather than guessing the last keyboard device.
const IME_DEVICE_ID: u64 = 1;
const FIRST_NATIVE_DEVICE_ID: u64 = 2;
const MOUSE_POINTER_ID: u64 = 1;
const FIRST_TOUCH_POINTER_ID: u64 = 2;

#[derive(Debug, PartialEq)]
pub(crate) struct NormalizedInput {
    pub(crate) event: InputEvent,
    pub(crate) origin: InputOrigin,
}

pub(crate) type InputBatch = Vec<NormalizedInput>;

pub(crate) struct InputNormalizer {
    surface: SurfaceId,
    scale: ScaleFactor,
    limits: LinuxShellLimits,
    epoch: Instant,
    next_sequence: Option<u64>,
    next_device_id: Option<u64>,
    next_pointer_id: Option<u64>,
    modifiers: Modifiers,
    devices: HashMap<DeviceId, InputDeviceId>,
    cursor_positions: HashMap<DeviceId, LogicalPoint>,
    cursor_inside: HashSet<DeviceId>,
    pending_enter: HashSet<DeviceId>,
    mouse_buttons: HashMap<DeviceId, u16>,
    continuous_scrolls: HashSet<DeviceId>,
    touches: HashMap<(DeviceId, u64), PointerId>,
    #[cfg(test)]
    elapsed_micros_override: Option<u128>,
}

impl InputNormalizer {
    pub(crate) fn new(surface: SurfaceId, scale: ScaleFactor, limits: LinuxShellLimits) -> Self {
        Self {
            surface,
            scale,
            limits,
            epoch: Instant::now(),
            next_sequence: Some(1),
            next_device_id: Some(FIRST_NATIVE_DEVICE_ID),
            next_pointer_id: Some(FIRST_TOUCH_POINTER_ID),
            modifiers: Modifiers::default(),
            devices: HashMap::with_capacity(limits.device_capacity),
            cursor_positions: HashMap::with_capacity(limits.device_capacity),
            cursor_inside: HashSet::with_capacity(limits.device_capacity),
            pending_enter: HashSet::with_capacity(limits.device_capacity),
            mouse_buttons: HashMap::with_capacity(limits.device_capacity),
            continuous_scrolls: HashSet::with_capacity(limits.device_capacity),
            touches: HashMap::with_capacity(limits.touch_capacity),
            #[cfg(test)]
            elapsed_micros_override: None,
        }
    }

    pub(crate) fn set_scale(&mut self, scale: ScaleFactor) {
        self.scale = scale;
    }

    pub(crate) fn modifiers_changed(&mut self, modifiers: ModifiersState) {
        self.modifiers = map_modifiers(modifiers);
    }

    pub(crate) fn cursor_entered(&mut self, device: DeviceId) -> Result<(), LinuxStopReason> {
        self.device_id(device)?;
        if !self.cursor_inside.contains(&device) {
            self.pending_enter.insert(device);
        }
        Ok(())
    }

    pub(crate) fn cursor_moved(
        &mut self,
        device: DeviceId,
        position: WinitPhysicalPosition<f64>,
    ) -> Result<InputBatch, LinuxStopReason> {
        let position = logical_position(position, self.scale)?;
        self.device_id(device)?;
        self.cursor_positions.insert(device, position);

        let mut batch = Vec::with_capacity(2);
        if self.pending_enter.remove(&device) {
            self.cursor_inside.insert(device);
            batch.push(self.pointer_input(device, PointerPhase::Enter, position, None)?);
        }
        batch.push(self.pointer_input(device, PointerPhase::Move, position, None)?);
        Ok(batch)
    }

    pub(crate) fn cursor_left(&mut self, device: DeviceId) -> Result<InputBatch, LinuxStopReason> {
        self.pending_enter.remove(&device);
        if !self.cursor_inside.remove(&device) {
            return Ok(Vec::new());
        }
        let Some(position) = self.cursor_positions.get(&device).copied() else {
            return Ok(Vec::new());
        };
        Ok(vec![self.pointer_input(
            device,
            PointerPhase::Leave,
            position,
            None,
        )?])
    }

    pub(crate) fn mouse_button(
        &mut self,
        device: DeviceId,
        state: ElementState,
        button: MouseButton,
    ) -> Result<InputBatch, LinuxStopReason> {
        self.device_id(device)?;
        let Some(position) = self.cursor_positions.get(&device).copied() else {
            return Ok(Vec::new());
        };
        let Some(button_bit) = mouse_button_bit(button) else {
            return Ok(Vec::new());
        };
        let buttons = self.mouse_buttons.entry(device).or_insert(0);
        let phase = match state {
            ElementState::Pressed if *buttons & button_bit == 0 => {
                *buttons |= button_bit;
                PointerPhase::Down
            }
            ElementState::Released if *buttons & button_bit != 0 => {
                *buttons &= !button_bit;
                PointerPhase::Up
            }
            ElementState::Pressed | ElementState::Released => return Ok(Vec::new()),
        };
        let buttons = *buttons;
        Ok(vec![self.pointer_input_with_buttons(
            device, phase, position, buttons, None,
        )?])
    }

    pub(crate) fn mouse_wheel(
        &mut self,
        device: DeviceId,
        delta: MouseScrollDelta,
        native_phase: TouchPhase,
    ) -> Result<InputBatch, LinuxStopReason> {
        self.device_id(device)?;
        let (delta, phase) = match delta {
            MouseScrollDelta::LineDelta(x, y) => {
                let vector = ScrollVector::new(f64::from(x), f64::from(y))
                    .map_err(|_| LinuxStopReason::InvalidPlatformGeometry)?;
                self.continuous_scrolls.remove(&device);
                (ScrollDelta::Lines(vector), ScrollPhase::Discrete)
            }
            MouseScrollDelta::PixelDelta(position) => {
                let vector =
                    ScrollVector::new(position.x / self.scale.get(), position.y / self.scale.get())
                        .map_err(|_| LinuxStopReason::InvalidPlatformGeometry)?;
                let phase = match native_phase {
                    TouchPhase::Started => {
                        self.continuous_scrolls.insert(device);
                        ScrollPhase::Begin
                    }
                    TouchPhase::Moved if self.continuous_scrolls.contains(&device) => {
                        ScrollPhase::Update
                    }
                    TouchPhase::Moved => {
                        self.continuous_scrolls.insert(device);
                        ScrollPhase::Begin
                    }
                    TouchPhase::Ended if self.continuous_scrolls.remove(&device) => {
                        ScrollPhase::End
                    }
                    TouchPhase::Cancelled if self.continuous_scrolls.remove(&device) => {
                        ScrollPhase::Cancel
                    }
                    TouchPhase::Ended | TouchPhase::Cancelled => return Ok(Vec::new()),
                };
                (ScrollDelta::Pixels(vector), phase)
            }
        };
        let metadata = self.metadata_for_device(device)?;
        Ok(vec![NormalizedInput {
            event: InputEvent::Scroll(ScrollEvent {
                metadata,
                delta,
                phase,
            }),
            origin: InputOrigin::Native,
        }])
    }

    pub(crate) fn keyboard(
        &mut self,
        device: DeviceId,
        physical_key: PhysicalKey,
        state: ElementState,
        location: WinitKeyLocation,
        repeat: bool,
        synthetic: bool,
    ) -> Result<InputBatch, LinuxStopReason> {
        if physical_key == PhysicalKey::Unidentified(NativeKeyCode::Unidentified) {
            return Ok(Vec::new());
        }
        let Some(scancode) = physical_key.to_scancode() else {
            return Ok(Vec::new());
        };
        let metadata = self.metadata_for_device(device)?;
        Ok(vec![NormalizedInput {
            event: InputEvent::Key(KeyEvent {
                metadata,
                physical_key: PhysicalKeyCode(scancode),
                state: match state {
                    ElementState::Pressed => KeyState::Down,
                    ElementState::Released => KeyState::Up,
                },
                location: match location {
                    WinitKeyLocation::Standard => KeyLocation::Standard,
                    WinitKeyLocation::Left => KeyLocation::Left,
                    WinitKeyLocation::Right => KeyLocation::Right,
                    WinitKeyLocation::Numpad => KeyLocation::Numpad,
                },
                repeat,
            }),
            origin: if synthetic {
                InputOrigin::Synthetic
            } else {
                InputOrigin::Native
            },
        }])
    }

    pub(crate) fn touch(&mut self, touch: Touch) -> Result<InputBatch, LinuxStopReason> {
        self.device_id(touch.device_id)?;
        let key = (touch.device_id, touch.id);
        let pointer = match touch.phase {
            TouchPhase::Started => {
                if self.touches.contains_key(&key) {
                    return Ok(Vec::new());
                }
                if self.touches.len() == self.limits.touch_capacity {
                    return Err(LinuxStopReason::TouchCapacityExhausted {
                        capacity: self.limits.touch_capacity,
                    });
                }
                let pointer = self.allocate_pointer_id()?;
                self.touches.insert(key, pointer);
                pointer
            }
            TouchPhase::Moved | TouchPhase::Ended | TouchPhase::Cancelled => {
                let Some(pointer) = self.touches.get(&key).copied() else {
                    return Ok(Vec::new());
                };
                pointer
            }
        };
        let position = logical_position(touch.location, self.scale)?;
        let pressure = map_force(touch.force)?;
        let phase = match touch.phase {
            TouchPhase::Started => PointerPhase::Down,
            TouchPhase::Moved => PointerPhase::Move,
            TouchPhase::Ended => PointerPhase::Up,
            TouchPhase::Cancelled => PointerPhase::Cancel,
        };
        let metadata = self.metadata_for_device(touch.device_id)?;
        let event = NormalizedInput {
            event: InputEvent::Pointer(PointerEvent {
                metadata,
                pointer,
                kind: PointerKind::Touch,
                phase,
                position,
                buttons: if matches!(phase, PointerPhase::Down | PointerPhase::Move) {
                    1
                } else {
                    0
                },
                pressure,
            }),
            origin: InputOrigin::Native,
        };
        if matches!(touch.phase, TouchPhase::Ended | TouchPhase::Cancelled) {
            self.touches.remove(&key);
        }
        Ok(vec![event])
    }

    pub(crate) fn ime_preedit(
        &self,
        text: String,
        selection: Option<(usize, usize)>,
    ) -> Result<BoundedImeText, LinuxStopReason> {
        BoundedImeText::new(text, selection, self.limits.ime_bytes)
            .map_err(|_| LinuxStopReason::InvalidImeText)
    }

    pub(crate) fn ime_commit(&mut self, text: String) -> Result<InputBatch, LinuxStopReason> {
        if text.is_empty() {
            return Ok(Vec::new());
        }
        if text.len() > self.limits.ime_bytes {
            return Err(LinuxStopReason::InvalidImeText);
        }
        let metadata = self.metadata_for_ime()?;
        let event =
            TextInputEvent::new(metadata, text).map_err(|_| LinuxStopReason::InvalidImeText)?;
        Ok(vec![NormalizedInput {
            event: InputEvent::Text(event),
            origin: InputOrigin::Native,
        }])
    }

    pub(crate) fn focus_lost(&mut self) {
        self.modifiers = Modifiers::default();
        self.cursor_positions.clear();
        self.pending_enter.clear();
        self.cursor_inside.clear();
        self.mouse_buttons.clear();
        self.continuous_scrolls.clear();
        self.touches.clear();
    }

    pub(crate) fn device_removed(&mut self, device: DeviceId) {
        self.devices.remove(&device);
        self.cursor_positions.remove(&device);
        self.cursor_inside.remove(&device);
        self.pending_enter.remove(&device);
        self.mouse_buttons.remove(&device);
        self.continuous_scrolls.remove(&device);
        self.touches
            .retain(|(candidate, _), _| *candidate != device);
    }

    fn pointer_input(
        &mut self,
        device: DeviceId,
        phase: PointerPhase,
        position: LogicalPoint,
        pressure: Option<UnitInterval>,
    ) -> Result<NormalizedInput, LinuxStopReason> {
        let buttons = self.mouse_buttons.get(&device).copied().unwrap_or(0);
        self.pointer_input_with_buttons(device, phase, position, buttons, pressure)
    }

    fn pointer_input_with_buttons(
        &mut self,
        device: DeviceId,
        phase: PointerPhase,
        position: LogicalPoint,
        buttons: u16,
        pressure: Option<UnitInterval>,
    ) -> Result<NormalizedInput, LinuxStopReason> {
        let metadata = self.metadata_for_device(device)?;
        Ok(NormalizedInput {
            event: InputEvent::Pointer(PointerEvent {
                metadata,
                pointer: PointerId::new(MOUSE_POINTER_ID)
                    .expect("the fixed mouse pointer ID is non-zero"),
                kind: PointerKind::Mouse,
                phase,
                position,
                buttons,
                pressure,
            }),
            origin: InputOrigin::Native,
        })
    }

    fn metadata_for_device(&mut self, device: DeviceId) -> Result<InputMetadata, LinuxStopReason> {
        let device = self.device_id(device)?;
        self.metadata(device)
    }

    fn metadata_for_ime(&mut self) -> Result<InputMetadata, LinuxStopReason> {
        let device =
            InputDeviceId::new(IME_DEVICE_ID).expect("the reserved IME device ID is non-zero");
        self.metadata(device)
    }

    fn metadata(&mut self, device: InputDeviceId) -> Result<InputMetadata, LinuxStopReason> {
        let raw_sequence = self
            .next_sequence
            .ok_or(LinuxStopReason::EventSequenceExhausted)?;
        let sequence =
            EventSequence::new(raw_sequence).ok_or(LinuxStopReason::EventSequenceExhausted)?;
        self.next_sequence = raw_sequence.checked_add(1);

        #[cfg(test)]
        let elapsed_micros = self
            .elapsed_micros_override
            .unwrap_or_else(|| self.epoch.elapsed().as_micros());
        #[cfg(not(test))]
        let elapsed_micros = self.epoch.elapsed().as_micros();
        let timestamp =
            u64::try_from(elapsed_micros).map_err(|_| LinuxStopReason::EventTimestampExhausted)?;

        Ok(InputMetadata {
            sequence,
            timestamp: EventTimestampMicros(timestamp),
            seat: SeatId::new(SEAT_ID).expect("the fixed Linux seat ID is non-zero"),
            device,
            surface: self.surface,
            modifiers: self.modifiers,
        })
    }

    fn device_id(&mut self, native: DeviceId) -> Result<InputDeviceId, LinuxStopReason> {
        if let Some(device) = self.devices.get(&native) {
            return Ok(*device);
        }
        if self.devices.len() == self.limits.device_capacity {
            return Err(LinuxStopReason::DeviceCapacityExhausted {
                capacity: self.limits.device_capacity,
            });
        }
        let raw = self
            .next_device_id
            .ok_or(LinuxStopReason::DeviceIdentityExhausted)?;
        let device = InputDeviceId::new(raw).ok_or(LinuxStopReason::DeviceIdentityExhausted)?;
        self.next_device_id = raw.checked_add(1);
        self.devices.insert(native, device);
        Ok(device)
    }

    fn allocate_pointer_id(&mut self) -> Result<PointerId, LinuxStopReason> {
        let raw = self
            .next_pointer_id
            .ok_or(LinuxStopReason::PointerIdentityExhausted)?;
        let pointer = PointerId::new(raw).ok_or(LinuxStopReason::PointerIdentityExhausted)?;
        self.next_pointer_id = raw.checked_add(1);
        Ok(pointer)
    }
}

pub(crate) fn physical_size(
    size: WinitPhysicalSize<u32>,
) -> Result<wild_buzzard_platform::PhysicalSize, LinuxStopReason> {
    wild_buzzard_platform::PhysicalSize::new(size.width, size.height)
        .map_err(|_| LinuxStopReason::InvalidPlatformGeometry)
}

pub(crate) fn scale_factor(value: f64) -> Result<ScaleFactor, LinuxStopReason> {
    ScaleFactor::new(value).map_err(|_| LinuxStopReason::InvalidPlatformGeometry)
}

fn logical_position(
    position: WinitPhysicalPosition<f64>,
    scale: ScaleFactor,
) -> Result<LogicalPoint, LinuxStopReason> {
    LogicalPoint::new(position.x / scale.get(), position.y / scale.get())
        .map_err(|_| LinuxStopReason::InvalidPlatformGeometry)
}

fn map_modifiers(state: ModifiersState) -> Modifiers {
    let mut modifiers = Modifiers::default();
    if state.shift_key() {
        modifiers = modifiers.union(Modifiers::SHIFT);
    }
    if state.control_key() {
        modifiers = modifiers.union(Modifiers::CONTROL);
    }
    if state.alt_key() {
        modifiers = modifiers.union(Modifiers::ALT);
    }
    if state.super_key() {
        modifiers = modifiers.union(Modifiers::SUPER);
    }
    modifiers
}

fn mouse_button_bit(button: MouseButton) -> Option<u16> {
    let index = match button {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::Back => 3,
        MouseButton::Forward => 4,
        MouseButton::Other(other) if other <= 10 => 5_u16.checked_add(other)?,
        MouseButton::Other(_) => return None,
    };
    1_u16.checked_shl(u32::from(index))
}

fn map_force(force: Option<Force>) -> Result<Option<UnitInterval>, LinuxStopReason> {
    let Some(force) = force else {
        return Ok(None);
    };
    let normalized = force.normalized();
    if !normalized.is_finite() || !(0.0..=1.0).contains(&normalized) {
        return Err(LinuxStopReason::InvalidTouchPressure);
    }
    UnitInterval::new(normalized as f32)
        .map(Some)
        .map_err(|_| LinuxStopReason::InvalidTouchPressure)
}

#[cfg(test)]
mod tests {
    use super::{
        FIRST_TOUCH_POINTER_ID, InputNormalizer, MOUSE_POINTER_ID, physical_size, scale_factor,
    };
    use crate::config::LinuxShellLimits;
    use crate::event::{InputOrigin, LinuxStopReason};
    use wild_buzzard_platform::{
        InputEvent, KeyState, PointerPhase, ScaleFactor, ScrollDelta, ScrollPhase,
        SurfaceIdAllocator, SurfaceNamespace,
    };
    use winit::dpi::{PhysicalPosition, PhysicalSize};
    use winit::event::{ElementState, Force, MouseButton, MouseScrollDelta, Touch, TouchPhase};
    use winit::keyboard::{KeyCode, KeyLocation, NativeKeyCode, PhysicalKey};

    fn make_normalizer() -> InputNormalizer {
        let mut allocator = SurfaceIdAllocator::new(SurfaceNamespace::new(71).unwrap());
        InputNormalizer::new(
            allocator.allocate().unwrap(),
            ScaleFactor::new(2.0).unwrap(),
            LinuxShellLimits::default(),
        )
    }

    #[test]
    fn geometry_rejects_invalid_platform_values() {
        assert!(physical_size(PhysicalSize::new(800, 600)).is_ok());
        assert_eq!(
            physical_size(PhysicalSize::new(40_000, 1)),
            Err(LinuxStopReason::InvalidPlatformGeometry)
        );
        assert_eq!(
            scale_factor(f64::NAN),
            Err(LinuxStopReason::InvalidPlatformGeometry)
        );
    }

    #[test]
    fn cursor_enter_waits_for_real_position_and_buttons_are_stateful() {
        let device = winit::event::DeviceId::dummy();
        let mut normalizer = make_normalizer();
        normalizer.cursor_entered(device).unwrap();
        let moved = normalizer
            .cursor_moved(device, PhysicalPosition::new(20.0, 10.0))
            .unwrap();
        assert_eq!(moved.len(), 2);
        let InputEvent::Pointer(entered) = &moved[0].event else {
            panic!("expected pointer enter");
        };
        assert_eq!(entered.phase, PointerPhase::Enter);
        assert_eq!((entered.position.x, entered.position.y), (10.0, 5.0));
        assert_eq!(entered.pointer.get(), MOUSE_POINTER_ID);

        let down = normalizer
            .mouse_button(device, ElementState::Pressed, MouseButton::Left)
            .unwrap();
        let InputEvent::Pointer(down) = &down[0].event else {
            panic!("expected pointer down");
        };
        assert_eq!(down.phase, PointerPhase::Down);
        assert_eq!(down.buttons, 1);
        assert!(
            normalizer
                .mouse_button(device, ElementState::Pressed, MouseButton::Left)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn keyboard_uses_linux_scancode_and_never_creates_text_input() {
        let mut normalizer = make_normalizer();
        let batch = normalizer
            .keyboard(
                winit::event::DeviceId::dummy(),
                PhysicalKey::Code(KeyCode::KeyA),
                ElementState::Pressed,
                KeyLocation::Standard,
                false,
                true,
            )
            .unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].origin, InputOrigin::Synthetic);
        let InputEvent::Key(key) = batch[0].event else {
            panic!("expected key event");
        };
        assert_eq!(key.state, KeyState::Down);

        assert!(
            normalizer
                .keyboard(
                    winit::event::DeviceId::dummy(),
                    PhysicalKey::Unidentified(NativeKeyCode::Unidentified),
                    ElementState::Pressed,
                    KeyLocation::Standard,
                    false,
                    false,
                )
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn pixel_scroll_starts_without_fabricating_a_prior_begin() {
        let mut normalizer = make_normalizer();
        let device = winit::event::DeviceId::dummy();
        let first = normalizer
            .mouse_wheel(
                device,
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(4.0, 6.0)),
                TouchPhase::Moved,
            )
            .unwrap();
        let InputEvent::Scroll(scroll) = first[0].event else {
            panic!("expected scroll");
        };
        assert_eq!(scroll.phase, ScrollPhase::Begin);
        let ScrollDelta::Pixels(delta) = scroll.delta else {
            panic!("expected pixel delta");
        };
        assert_eq!((delta.x, delta.y), (2.0, 3.0));

        let next = normalizer
            .mouse_wheel(
                device,
                MouseScrollDelta::PixelDelta(PhysicalPosition::new(2.0, 2.0)),
                TouchPhase::Moved,
            )
            .unwrap();
        let InputEvent::Scroll(scroll) = next[0].event else {
            panic!("expected scroll");
        };
        assert_eq!(scroll.phase, ScrollPhase::Update);
    }

    #[test]
    fn touch_pointer_ids_never_collide_with_the_mouse() {
        assert_ne!(FIRST_TOUCH_POINTER_ID, MOUSE_POINTER_ID);
        let mut normalizer = make_normalizer();
        let touch = Touch {
            device_id: winit::event::DeviceId::dummy(),
            phase: TouchPhase::Started,
            location: PhysicalPosition::new(10.0, 20.0),
            force: Some(Force::Normalized(0.5)),
            id: 99,
        };
        let batch = normalizer.touch(touch).unwrap();
        let InputEvent::Pointer(pointer) = batch[0].event else {
            panic!("expected touch pointer");
        };
        assert_eq!(pointer.pointer.get(), FIRST_TOUCH_POINTER_ID);
        assert_ne!(pointer.pointer.get(), MOUSE_POINTER_ID);
    }

    #[test]
    fn all_checked_exhaustions_are_distinct_and_do_not_wrap() {
        let device = winit::event::DeviceId::dummy();

        let mut normalizer = make_normalizer();
        normalizer.next_sequence = None;
        assert_eq!(
            normalizer.keyboard(
                device,
                PhysicalKey::Code(KeyCode::KeyA),
                ElementState::Pressed,
                KeyLocation::Standard,
                false,
                false,
            ),
            Err(LinuxStopReason::EventSequenceExhausted)
        );

        let mut normalizer = make_normalizer();
        normalizer.elapsed_micros_override = Some(u128::from(u64::MAX) + 1);
        assert_eq!(
            normalizer.keyboard(
                device,
                PhysicalKey::Code(KeyCode::KeyA),
                ElementState::Pressed,
                KeyLocation::Standard,
                false,
                false,
            ),
            Err(LinuxStopReason::EventTimestampExhausted)
        );

        let mut normalizer = make_normalizer();
        normalizer.next_device_id = None;
        assert_eq!(
            normalizer.cursor_entered(device),
            Err(LinuxStopReason::DeviceIdentityExhausted)
        );

        let mut normalizer = make_normalizer();
        normalizer.next_pointer_id = None;
        assert_eq!(
            normalizer.touch(Touch {
                device_id: device,
                phase: TouchPhase::Started,
                location: PhysicalPosition::new(1.0, 1.0),
                force: None,
                id: 1,
            }),
            Err(LinuxStopReason::PointerIdentityExhausted)
        );
    }
}
