//! Platform-neutral geometry, input, and surface contracts.
//!
//! These types contain no native window pointers or toolkit handles. Future
//! concrete adapters target Linux x86_64 with Wayland and X11 as needed.

#![forbid(unsafe_code)]

use std::error::Error;
use std::fmt;
use std::num::NonZeroU64;
use wild_buzzard_handles::{Arena, Handle, InsertError};

/// Maximum accepted physical width or height for a surface.
pub const MAX_SURFACE_DIMENSION: u32 = 32_768;

/// Maximum accepted physical pixel count for a surface.
pub const MAX_SURFACE_PIXELS: u64 = 268_435_456;

/// Maximum accepted device scale factor.
pub const MAX_SCALE_FACTOR: f64 = 64.0;

/// Maximum absolute logical coordinate accepted at a platform boundary.
pub const MAX_LOGICAL_COORDINATE: f64 = 1_000_000_000.0;

/// Maximum logical width or height accepted at a platform boundary.
pub const MAX_LOGICAL_EXTENT: f64 = 1_000_000_000.0;

/// Maximum UTF-8 bytes accepted in one committed text input event.
pub const MAX_TEXT_INPUT_BYTES: usize = 64 * 1024;

#[derive(Debug)]
struct SurfaceRecord;

/// A non-zero namespace separating surface allocators and process roles.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SurfaceNamespace(NonZeroU64);

impl SurfaceNamespace {
    /// Creates a namespace, rejecting the reserved zero value.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the wire value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// A typed generational identity for a compositor surface.
pub struct SurfaceId {
    namespace: SurfaceNamespace,
    handle: Handle<SurfaceRecord>,
}

impl SurfaceId {
    /// Returns the allocator/process namespace.
    #[must_use]
    pub const fn namespace(self) -> SurfaceNamespace {
        self.namespace
    }

    /// Returns the allocator-local slot for diagnostics and serialization.
    #[must_use]
    pub const fn slot(self) -> u32 {
        self.handle.slot()
    }

    /// Returns the non-zero generation for diagnostics and serialization.
    #[must_use]
    pub const fn generation(self) -> u32 {
        self.handle.generation()
    }
}

impl Clone for SurfaceId {
    fn clone(&self) -> Self {
        *self
    }
}

impl Copy for SurfaceId {}

impl fmt::Debug for SurfaceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SurfaceId")
            .field("namespace", &self.namespace)
            .field("slot", &self.slot())
            .field("generation", &self.generation())
            .finish()
    }
}

impl PartialEq for SurfaceId {
    fn eq(&self, other: &Self) -> bool {
        self.namespace == other.namespace && self.handle == other.handle
    }
}

impl Eq for SurfaceId {}

impl std::hash::Hash for SurfaceId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.namespace.hash(state);
        self.handle.hash(state);
    }
}

/// Allocates and retires typed surface identities without creating a surface.
#[derive(Debug)]
pub struct SurfaceIdAllocator {
    namespace: SurfaceNamespace,
    records: Arena<SurfaceRecord>,
}

impl SurfaceIdAllocator {
    /// Creates an empty identity allocator.
    #[must_use]
    pub fn new(namespace: SurfaceNamespace) -> Self {
        Self {
            namespace,
            records: Arena::new(),
        }
    }

    /// Allocates a fresh live surface identity.
    pub fn allocate(&mut self) -> Result<SurfaceId, SurfaceIdError> {
        self.records
            .try_insert(SurfaceRecord)
            .map(|handle| SurfaceId {
                namespace: self.namespace,
                handle,
            })
            .map_err(|InsertError::CapacityExhausted| SurfaceIdError::CapacityExhausted)
    }

    /// Retires an identity. A stale or foreign allocator identity is rejected.
    pub fn release(&mut self, id: SurfaceId) -> Result<(), SurfaceIdError> {
        if id.namespace != self.namespace {
            return Err(SurfaceIdError::ForeignNamespace {
                expected: self.namespace,
                actual: id.namespace,
            });
        }
        self.records
            .remove(id.handle)
            .map(|_| ())
            .ok_or(SurfaceIdError::StaleIdentity)
    }

    /// Returns whether this allocator currently owns the identity.
    #[must_use]
    pub fn is_live(&self, id: SurfaceId) -> bool {
        id.namespace == self.namespace && self.records.contains(id.handle)
    }
}

/// A surface identity allocation or lifetime error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceIdError {
    /// All representable slots have been allocated or retired.
    CapacityExhausted,
    /// The slot is absent or its generation has been invalidated.
    StaleIdentity,
    /// The identity belongs to another allocator or process namespace.
    ForeignNamespace {
        /// Namespace of this allocator.
        expected: SurfaceNamespace,
        /// Namespace carried by the identity.
        actual: SurfaceNamespace,
    },
}

impl fmt::Display for SurfaceIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CapacityExhausted => formatter.write_str("surface identity capacity exhausted"),
            Self::StaleIdentity => formatter.write_str("surface identity is stale"),
            Self::ForeignNamespace { expected, actual } => write!(
                formatter,
                "foreign surface namespace: expected {}, received {}",
                expected.get(),
                actual.get()
            ),
        }
    }
}

impl Error for SurfaceIdError {}

/// A logical point measured before device scaling.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LogicalPoint {
    /// Horizontal coordinate.
    pub x: f64,
    /// Vertical coordinate.
    pub y: f64,
}

impl LogicalPoint {
    /// Creates a point after rejecting non-finite coordinates.
    pub fn new(x: f64, y: f64) -> Result<Self, GeometryError> {
        validate_coordinate(x, "x")?;
        validate_coordinate(y, "y")?;
        Ok(Self { x, y })
    }

    /// Converts to physical pixels using checked nearest-pixel rounding.
    pub fn to_physical(self, scale: ScaleFactor) -> Result<PhysicalPoint, GeometryError> {
        Ok(PhysicalPoint {
            x: round_to_i32(self.x * scale.get(), "x")?,
            y: round_to_i32(self.y * scale.get(), "y")?,
        })
    }
}

/// A non-negative logical size measured before device scaling.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LogicalSize {
    /// Horizontal extent.
    pub width: f64,
    /// Vertical extent.
    pub height: f64,
}

impl LogicalSize {
    /// Creates a finite, non-negative size.
    pub fn new(width: f64, height: f64) -> Result<Self, GeometryError> {
        validate_extent(width, "width")?;
        validate_extent(height, "height")?;
        Ok(Self { width, height })
    }

    /// Converts to a checked physical-pixel size.
    pub fn to_physical(self, scale: ScaleFactor) -> Result<PhysicalSize, GeometryError> {
        let width = round_to_u32(self.width * scale.get(), "width")?;
        let height = round_to_u32(self.height * scale.get(), "height")?;
        PhysicalSize::new(width, height)
    }
}

/// A logical rectangle represented by an origin and non-negative size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LogicalRect {
    /// Rectangle origin.
    pub origin: LogicalPoint,
    /// Rectangle extent.
    pub size: LogicalSize,
}

impl LogicalRect {
    /// Creates a rectangle from already validated parts.
    #[must_use]
    pub const fn new(origin: LogicalPoint, size: LogicalSize) -> Self {
        Self { origin, size }
    }

    /// Returns whether a point lies within the half-open rectangle.
    #[must_use]
    pub fn contains(self, point: LogicalPoint) -> bool {
        point.x >= self.origin.x
            && point.y >= self.origin.y
            && point.x < self.origin.x + self.size.width
            && point.y < self.origin.y + self.size.height
    }
}

/// A physical integer point measured in device pixels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalPoint {
    /// Horizontal coordinate.
    pub x: i32,
    /// Vertical coordinate.
    pub y: i32,
}

/// A checked physical surface extent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalSize {
    /// Width in physical pixels.
    pub width: u32,
    /// Height in physical pixels.
    pub height: u32,
}

impl PhysicalSize {
    /// Creates a size subject to per-axis and total-pixel limits.
    pub fn new(width: u32, height: u32) -> Result<Self, GeometryError> {
        if width > MAX_SURFACE_DIMENSION || height > MAX_SURFACE_DIMENSION {
            return Err(GeometryError::SurfaceDimensionTooLarge {
                width,
                height,
                maximum: MAX_SURFACE_DIMENSION,
            });
        }
        let pixels = u64::from(width) * u64::from(height);
        if pixels > MAX_SURFACE_PIXELS {
            return Err(GeometryError::SurfaceAreaTooLarge {
                pixels,
                maximum: MAX_SURFACE_PIXELS,
            });
        }
        Ok(Self { width, height })
    }

    /// Returns the checked total pixel count.
    #[must_use]
    pub const fn pixels(self) -> u64 {
        self.width as u64 * self.height as u64
    }
}

/// A finite positive conversion from logical to physical pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScaleFactor(f64);

impl ScaleFactor {
    /// Creates a scale in `(0, MAX_SCALE_FACTOR]`.
    pub fn new(value: f64) -> Result<Self, GeometryError> {
        if !value.is_finite() || value <= 0.0 || value > MAX_SCALE_FACTOR {
            return Err(GeometryError::InvalidScale {
                value,
                maximum: MAX_SCALE_FACTOR,
            });
        }
        Ok(Self(value))
    }

    /// Returns the validated factor.
    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }
}

/// Invalid geometry received from a platform or subsystem boundary.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GeometryError {
    /// A floating-point field was NaN or infinite.
    NonFinite {
        /// Static field name.
        field: &'static str,
    },
    /// A size extent was negative.
    NegativeExtent {
        /// Static field name.
        field: &'static str,
        /// Rejected value.
        value: f64,
    },
    /// A coordinate or extent cannot be represented by its physical integer.
    OutOfRange {
        /// Static field name.
        field: &'static str,
    },
    /// One surface dimension exceeds the hard bound.
    SurfaceDimensionTooLarge {
        /// Requested width.
        width: u32,
        /// Requested height.
        height: u32,
        /// Per-axis maximum.
        maximum: u32,
    },
    /// The total number of pixels exceeds the hard bound.
    SurfaceAreaTooLarge {
        /// Requested pixels.
        pixels: u64,
        /// Maximum pixels.
        maximum: u64,
    },
    /// A scale was non-finite, non-positive, or unreasonably large.
    InvalidScale {
        /// Rejected value.
        value: f64,
        /// Maximum accepted value.
        maximum: f64,
    },
    /// A logical coordinate exceeds the explicit platform-contract range.
    CoordinateTooLarge {
        /// Static field name.
        field: &'static str,
        /// Rejected value.
        value: f64,
        /// Maximum accepted absolute value.
        maximum_absolute: f64,
    },
    /// A logical extent exceeds the explicit platform-contract range.
    ExtentTooLarge {
        /// Static field name.
        field: &'static str,
        /// Rejected value.
        value: f64,
        /// Maximum accepted extent.
        maximum: f64,
    },
}

impl fmt::Display for GeometryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFinite { field } => write!(formatter, "geometry field {field} is not finite"),
            Self::NegativeExtent { field, value } => {
                write!(formatter, "geometry extent {field} is negative: {value}")
            }
            Self::OutOfRange { field } => {
                write!(
                    formatter,
                    "geometry field {field} is outside its integer range"
                )
            }
            Self::SurfaceDimensionTooLarge {
                width,
                height,
                maximum,
            } => write!(
                formatter,
                "surface {width}x{height} exceeds per-axis maximum {maximum}"
            ),
            Self::SurfaceAreaTooLarge { pixels, maximum } => {
                write!(formatter, "surface area {pixels} exceeds maximum {maximum}")
            }
            Self::InvalidScale { value, maximum } => write!(
                formatter,
                "scale factor {value} is not finite and within (0, {maximum}]"
            ),
            Self::CoordinateTooLarge {
                field,
                value,
                maximum_absolute,
            } => write!(
                formatter,
                "logical coordinate {field}={value} exceeds absolute maximum {maximum_absolute}"
            ),
            Self::ExtentTooLarge {
                field,
                value,
                maximum,
            } => write!(
                formatter,
                "logical extent {field}={value} exceeds maximum {maximum}"
            ),
        }
    }
}

impl Error for GeometryError {}

fn validate_finite(value: f64, field: &'static str) -> Result<(), GeometryError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(GeometryError::NonFinite { field })
    }
}

fn validate_coordinate(value: f64, field: &'static str) -> Result<(), GeometryError> {
    validate_finite(value, field)?;
    if value.abs() > MAX_LOGICAL_COORDINATE {
        Err(GeometryError::CoordinateTooLarge {
            field,
            value,
            maximum_absolute: MAX_LOGICAL_COORDINATE,
        })
    } else {
        Ok(())
    }
}

fn validate_extent(value: f64, field: &'static str) -> Result<(), GeometryError> {
    validate_finite(value, field)?;
    if value < 0.0 {
        Err(GeometryError::NegativeExtent { field, value })
    } else if value > MAX_LOGICAL_EXTENT {
        Err(GeometryError::ExtentTooLarge {
            field,
            value,
            maximum: MAX_LOGICAL_EXTENT,
        })
    } else {
        Ok(())
    }
}

fn round_to_i32(value: f64, field: &'static str) -> Result<i32, GeometryError> {
    if !value.is_finite()
        || value.round() < f64::from(i32::MIN)
        || value.round() > f64::from(i32::MAX)
    {
        Err(GeometryError::OutOfRange { field })
    } else {
        Ok(value.round() as i32)
    }
}

fn round_to_u32(value: f64, field: &'static str) -> Result<u32, GeometryError> {
    if !value.is_finite() || value < 0.0 || value.round() > f64::from(u32::MAX) {
        Err(GeometryError::OutOfRange { field })
    } else {
        Ok(value.round() as u32)
    }
}

macro_rules! non_zero_id {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(NonZeroU64);

        impl $name {
            /// Creates an identifier, rejecting the reserved zero value.
            #[must_use]
            pub const fn new(value: u64) -> Option<Self> {
                match NonZeroU64::new(value) {
                    Some(value) => Some(Self(value)),
                    None => None,
                }
            }

            /// Returns the integer identity.
            #[must_use]
            pub const fn get(self) -> u64 {
                self.0.get()
            }
        }
    };
}

non_zero_id!(InputDeviceId, "A process-scoped input-device identity.");
non_zero_id!(SeatId, "A process-scoped Linux input-seat identity.");
non_zero_id!(PointerId, "A non-zero pointer identity within one seat.");
non_zero_id!(
    EventSequence,
    "A monotonically assigned input-event identity."
);

/// A monotonic timestamp supplied by the platform adapter, in microseconds.
///
/// The epoch is adapter-local and must not be interpreted as wall-clock time.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct EventTimestampMicros(pub u64);

/// Valid keyboard modifier bits.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct Modifiers(u16);

impl Modifiers {
    /// Shift key.
    pub const SHIFT: Self = Self(1 << 0);
    /// Control key.
    pub const CONTROL: Self = Self(1 << 1);
    /// Alt key.
    pub const ALT: Self = Self(1 << 2);
    /// Super/Meta key.
    pub const SUPER: Self = Self(1 << 3);
    /// Caps Lock state.
    pub const CAPS_LOCK: Self = Self(1 << 4);
    /// Num Lock state.
    pub const NUM_LOCK: Self = Self(1 << 5);

    const KNOWN: u16 = Self::SHIFT.0
        | Self::CONTROL.0
        | Self::ALT.0
        | Self::SUPER.0
        | Self::CAPS_LOCK.0
        | Self::NUM_LOCK.0;

    /// Validates raw modifier bits.
    pub fn from_bits(bits: u16) -> Result<Self, InputError> {
        let unknown = bits & !Self::KNOWN;
        if unknown == 0 {
            Ok(Self(bits))
        } else {
            Err(InputError::UnknownModifierBits { bits: unknown })
        }
    }

    /// Combines modifier states.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Returns whether every bit in `other` is present.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Returns validated bits.
    #[must_use]
    pub const fn bits(self) -> u16 {
        self.0
    }
}

/// A finite scalar in the inclusive range zero through one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UnitInterval(f32);

impl UnitInterval {
    /// Validates a normalized scalar.
    pub fn new(value: f32) -> Result<Self, InputError> {
        if value.is_finite() && (0.0..=1.0).contains(&value) {
            Ok(Self(value))
        } else {
            Err(InputError::InvalidUnitInterval { value })
        }
    }

    /// Returns the validated scalar.
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }
}

/// Stable metadata shared by all input events.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputMetadata {
    /// Event sequence assigned by the adapter.
    pub sequence: EventSequence,
    /// Adapter-local monotonic timestamp.
    pub timestamp: EventTimestampMicros,
    /// Linux input seat.
    pub seat: SeatId,
    /// Physical or virtual device.
    pub device: InputDeviceId,
    /// Target surface at dispatch time.
    pub surface: SurfaceId,
    /// Modifier snapshot.
    pub modifiers: Modifiers,
}

/// Input source category independent of Wayland/X11 native values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PointerKind {
    /// Mouse or mouse-like device.
    Mouse,
    /// Direct touch contact.
    Touch,
    /// Pen or stylus.
    Pen,
    /// Device category not understood by this version.
    Other(u16),
}

/// Pointer transition independent of native event numbers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PointerPhase {
    /// Pointer entered a target surface.
    Enter,
    /// Pointer moved.
    Move,
    /// Button/contact became active.
    Down,
    /// Button/contact became inactive.
    Up,
    /// Gesture/contact was cancelled.
    Cancel,
    /// Pointer left a target surface.
    Leave,
}

/// A platform-normalized pointer event.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointerEvent {
    /// Shared routing and timing fields.
    pub metadata: InputMetadata,
    /// Pointer identity within the seat.
    pub pointer: PointerId,
    /// Pointer source category.
    pub kind: PointerKind,
    /// Transition phase.
    pub phase: PointerPhase,
    /// Logical position relative to the surface.
    pub position: LogicalPoint,
    /// Pressed button bitset in Wild Buzzard canonical numbering.
    pub buttons: u16,
    /// Optional normalized pressure.
    pub pressure: Option<UnitInterval>,
}

/// A finite two-dimensional scroll delta.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollVector {
    /// Horizontal delta.
    pub x: f64,
    /// Vertical delta.
    pub y: f64,
}

impl ScrollVector {
    /// Creates a finite bounded scroll delta.
    pub fn new(x: f64, y: f64) -> Result<Self, GeometryError> {
        validate_coordinate(x, "scroll_x")?;
        validate_coordinate(y, "scroll_y")?;
        Ok(Self { x, y })
    }
}

/// Unit and value of a normalized wheel or touchpad scroll.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScrollDelta {
    /// Logical CSS-like pixels.
    Pixels(ScrollVector),
    /// Platform-normalized text lines.
    Lines(ScrollVector),
    /// Whole viewport pages.
    Pages(ScrollVector),
}

/// Phase of a potentially multi-event scroll gesture.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScrollPhase {
    /// Standalone wheel tick without gesture state.
    Discrete,
    /// Continuous gesture began.
    Begin,
    /// Continuous gesture changed.
    Update,
    /// Continuous gesture ended normally.
    End,
    /// Continuous gesture was cancelled.
    Cancel,
}

/// A normalized wheel or touchpad scroll event.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollEvent {
    /// Shared routing and timing fields.
    pub metadata: InputMetadata,
    /// Unit-bearing scroll delta.
    pub delta: ScrollDelta,
    /// Gesture phase.
    pub phase: ScrollPhase,
}

/// Key direction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyState {
    /// Key became active.
    Down,
    /// Key became inactive.
    Up,
}

/// Position of a key within a keyboard layout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyLocation {
    /// No special location.
    Standard,
    /// Left-side modifier.
    Left,
    /// Right-side modifier.
    Right,
    /// Numeric keypad.
    Numpad,
}

/// A canonical physical key code assigned by a future Linux keymap adapter.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PhysicalKeyCode(pub u32);

/// A normalized keyboard event. Text composition is a separate event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyEvent {
    /// Shared routing and timing fields.
    pub metadata: InputMetadata,
    /// Physical key identity.
    pub physical_key: PhysicalKeyCode,
    /// Press or release.
    pub state: KeyState,
    /// Key location.
    pub location: KeyLocation,
    /// Whether this is an auto-repeat event.
    pub repeat: bool,
}

/// Validated committed text from an input method.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextInputEvent {
    /// Shared routing and timing fields.
    pub metadata: InputMetadata,
    text: String,
}

impl TextInputEvent {
    /// Creates an event after enforcing the UTF-8 byte bound.
    pub fn new(metadata: InputMetadata, text: String) -> Result<Self, InputError> {
        if text.len() > MAX_TEXT_INPUT_BYTES {
            return Err(InputError::TextTooLong {
                actual: text.len(),
                maximum: MAX_TEXT_INPUT_BYTES,
            });
        }
        Ok(Self { metadata, text })
    }

    /// Returns the committed UTF-8 text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

/// A complete platform-neutral input event.
#[derive(Clone, Debug, PartialEq)]
pub enum InputEvent {
    /// Pointer event.
    Pointer(PointerEvent),
    /// Wheel or touchpad scroll event.
    Scroll(ScrollEvent),
    /// Keyboard event.
    Key(KeyEvent),
    /// Committed input-method text.
    Text(TextInputEvent),
}

/// Invalid input data rejected at the platform boundary.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum InputError {
    /// Modifier bits are not defined by this contract version.
    UnknownModifierBits {
        /// Unsupported bits.
        bits: u16,
    },
    /// Pressure or another normalized scalar is outside `[0, 1]` or non-finite.
    InvalidUnitInterval {
        /// Rejected value.
        value: f32,
    },
    /// One committed text event exceeded the explicit byte bound.
    TextTooLong {
        /// Received UTF-8 byte count.
        actual: usize,
        /// Maximum UTF-8 byte count.
        maximum: usize,
    },
}

impl fmt::Display for InputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownModifierBits { bits } => {
                write!(formatter, "unknown modifier bits: {bits:#06x}")
            }
            Self::InvalidUnitInterval { value } => {
                write!(
                    formatter,
                    "normalized input scalar is outside [0, 1]: {value}"
                )
            }
            Self::TextTooLong { actual, maximum } => write!(
                formatter,
                "text input contains {actual} UTF-8 bytes, above maximum {maximum}"
            ),
        }
    }
}

impl Error for InputError {}

/// Renderer-facing surface pixel format.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PixelFormat {
    /// Eight-bit red, green, blue, alpha channels in sRGB.
    Rgba8Srgb,
    /// Eight-bit blue, green, red, alpha channels in sRGB.
    Bgra8Srgb,
    /// Sixteen-bit floating-point red, green, blue, alpha channels.
    Rgba16Float,
}

/// High-level purpose of a surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceRole {
    /// Top-level browser window content.
    Window,
    /// Popup, menu, or transient child surface.
    Popup,
    /// Renderer-owned off-screen target.
    Offscreen,
}

/// Validated metadata shared with a future renderer or compositor adapter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SurfaceDescriptor {
    /// Generational surface identity.
    pub id: SurfaceId,
    /// Bounded physical extent.
    pub size: PhysicalSize,
    /// Logical-to-physical factor.
    pub scale: ScaleFactor,
    /// Pixel storage format.
    pub format: PixelFormat,
    /// Surface purpose.
    pub role: SurfaceRole,
}

#[cfg(test)]
mod tests {
    use super::{
        EventSequence, EventTimestampMicros, GeometryError, InputDeviceId, InputMetadata,
        LogicalPoint, LogicalRect, LogicalSize, MAX_LOGICAL_COORDINATE, MAX_LOGICAL_EXTENT,
        MAX_SURFACE_DIMENSION, MAX_TEXT_INPUT_BYTES, Modifiers, PhysicalSize, ScaleFactor,
        ScrollVector, SeatId, SurfaceId, SurfaceIdAllocator, SurfaceIdError, SurfaceNamespace,
        TextInputEvent, UnitInterval,
    };

    #[test]
    fn stale_surface_identity_is_rejected_after_slot_reuse() {
        let mut allocator = SurfaceIdAllocator::new(SurfaceNamespace::new(1).unwrap());
        let first = allocator.allocate().unwrap();
        assert!(allocator.is_live(first));
        allocator.release(first).unwrap();
        assert_eq!(allocator.release(first), Err(SurfaceIdError::StaleIdentity));

        let second = allocator.allocate().unwrap();
        assert_eq!(first.slot(), second.slot());
        assert_ne!(first.generation(), second.generation());
        assert!(!allocator.is_live(first));
        assert!(allocator.is_live(second));

        let mut foreign = SurfaceIdAllocator::new(SurfaceNamespace::new(2).unwrap());
        assert!(matches!(
            foreign.release(second),
            Err(SurfaceIdError::ForeignNamespace { .. })
        ));
    }

    #[test]
    fn geometry_rejects_non_finite_negative_and_oversized_values() {
        assert_eq!(
            LogicalPoint::new(f64::NAN, 0.0),
            Err(GeometryError::NonFinite { field: "x" })
        );
        assert_eq!(
            LogicalSize::new(-1.0, 4.0),
            Err(GeometryError::NegativeExtent {
                field: "width",
                value: -1.0,
            })
        );
        assert!(ScaleFactor::new(0.0).is_err());
        assert!(ScaleFactor::new(f64::INFINITY).is_err());
        assert!(PhysicalSize::new(MAX_SURFACE_DIMENSION + 1, 1).is_err());
        assert!(PhysicalSize::new(MAX_SURFACE_DIMENSION, MAX_SURFACE_DIMENSION).is_err());
        assert!(LogicalPoint::new(MAX_LOGICAL_COORDINATE + 1.0, 0.0).is_err());
        assert!(LogicalSize::new(MAX_LOGICAL_EXTENT + 1.0, 1.0).is_err());
        assert!(ScrollVector::new(f64::INFINITY, 0.0).is_err());
    }

    #[test]
    fn logical_geometry_converts_and_uses_half_open_rectangles() {
        let scale = ScaleFactor::new(1.5).unwrap();
        assert_eq!(
            LogicalPoint::new(2.0, -2.0)
                .unwrap()
                .to_physical(scale)
                .unwrap(),
            super::PhysicalPoint { x: 3, y: -3 }
        );
        assert_eq!(
            LogicalSize::new(20.0, 10.0)
                .unwrap()
                .to_physical(scale)
                .unwrap(),
            PhysicalSize::new(30, 15).unwrap()
        );

        let rect = LogicalRect::new(
            LogicalPoint::new(10.0, 20.0).unwrap(),
            LogicalSize::new(5.0, 5.0).unwrap(),
        );
        assert!(rect.contains(LogicalPoint::new(10.0, 20.0).unwrap()));
        assert!(!rect.contains(LogicalPoint::new(15.0, 20.0).unwrap()));
    }

    fn metadata(surface: SurfaceId) -> InputMetadata {
        InputMetadata {
            sequence: EventSequence::new(1).unwrap(),
            timestamp: EventTimestampMicros(0),
            seat: SeatId::new(1).unwrap(),
            device: InputDeviceId::new(1).unwrap(),
            surface,
            modifiers: Modifiers::SHIFT.union(Modifiers::CONTROL),
        }
    }

    #[test]
    fn input_values_are_explicitly_bounded() {
        assert!(UnitInterval::new(0.0).is_ok());
        assert!(UnitInterval::new(1.0).is_ok());
        assert!(UnitInterval::new(f32::NAN).is_err());
        assert!(UnitInterval::new(1.1).is_err());
        assert!(Modifiers::from_bits(0x8000).is_err());

        let mut allocator = SurfaceIdAllocator::new(SurfaceNamespace::new(3).unwrap());
        let surface = allocator.allocate().unwrap();
        let text = "x".repeat(MAX_TEXT_INPUT_BYTES + 1);
        assert!(TextInputEvent::new(metadata(surface), text).is_err());
        assert_eq!(
            TextInputEvent::new(metadata(surface), String::from("buzzard"))
                .unwrap()
                .text(),
            "buzzard"
        );
    }

    #[test]
    fn platform_contract_types_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SurfaceId>();
        assert_send_sync::<SurfaceIdAllocator>();
        assert_send_sync::<super::InputEvent>();
        assert_send_sync::<super::SurfaceDescriptor>();
    }
}
