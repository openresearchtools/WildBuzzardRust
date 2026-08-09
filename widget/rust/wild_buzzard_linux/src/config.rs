use std::error::Error;
use std::fmt;

use wild_buzzard_platform::{LogicalSize, PhysicalSize, PixelFormat, SurfaceNamespace};

/// Maximum number of UTF-8 bytes accepted for a window title.
pub const MAX_TITLE_BYTES: usize = 1_024;
/// Maximum number of ASCII bytes accepted for a Linux application ID.
pub const MAX_APPLICATION_ID_BYTES: usize = 255;
/// Hard maximum for the ordinary event queue.
pub const MAX_EVENT_CAPACITY: usize = 4_096;
/// Hard maximum for simultaneously known input devices.
pub const MAX_DEVICE_CAPACITY: usize = 256;
/// Hard maximum for simultaneously active touch contacts.
pub const MAX_TOUCH_CAPACITY: usize = 1_024;
/// Hard maximum for one IME preedit or commit payload.
pub const MAX_IME_BYTES: usize = 64 * 1_024;

const MIN_EVENT_CAPACITY: usize = 8;
const MIN_IDENTITY_CAPACITY: usize = 1;
const DEFAULT_EVENT_CAPACITY: usize = 256;
const DEFAULT_DEVICE_CAPACITY: usize = 32;
const DEFAULT_TOUCH_CAPACITY: usize = 64;
const DEFAULT_IME_BYTES: usize = 16 * 1_024;

/// Requested display backend for the Linux event loop.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LinuxBackendPreference {
    /// Prefer Wayland when a Wayland display is present, otherwise use X11.
    #[default]
    Auto,
    /// Require a Wayland display.
    Wayland,
    /// Require an X11 display.
    X11,
}

/// Bounded resources owned by one window shell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LinuxShellLimits {
    /// Ordinary event queue capacity; the terminal slot is separate.
    pub event_capacity: usize,
    /// Maximum simultaneously registered input devices.
    pub device_capacity: usize,
    /// Maximum simultaneously active touch contacts.
    pub touch_capacity: usize,
    /// Maximum UTF-8 bytes in one IME payload.
    pub ime_bytes: usize,
}

impl Default for LinuxShellLimits {
    fn default() -> Self {
        Self {
            event_capacity: DEFAULT_EVENT_CAPACITY,
            device_capacity: DEFAULT_DEVICE_CAPACITY,
            touch_capacity: DEFAULT_TOUCH_CAPACITY,
            ime_bytes: DEFAULT_IME_BYTES,
        }
    }
}

impl LinuxShellLimits {
    fn validate(self) -> Result<Self, ConfigError> {
        validate_capacity(
            "event_capacity",
            self.event_capacity,
            MIN_EVENT_CAPACITY,
            MAX_EVENT_CAPACITY,
        )?;
        validate_capacity(
            "device_capacity",
            self.device_capacity,
            MIN_IDENTITY_CAPACITY,
            MAX_DEVICE_CAPACITY,
        )?;
        validate_capacity(
            "touch_capacity",
            self.touch_capacity,
            MIN_IDENTITY_CAPACITY,
            MAX_TOUCH_CAPACITY,
        )?;
        validate_capacity(
            "ime_bytes",
            self.ime_bytes,
            MIN_IDENTITY_CAPACITY,
            MAX_IME_BYTES,
        )?;
        Ok(self)
    }
}

/// Caller-supplied settings for one top-level Wild Buzzard window.
#[derive(Clone, Debug, PartialEq)]
pub struct LinuxShellConfig {
    /// UTF-8 title shown by the window manager.
    pub title: String,
    /// Reverse-domain Linux desktop application identity.
    pub application_id: String,
    /// Initial client-area size in logical pixels.
    pub initial_size: LogicalSize,
    /// Desired renderer pixel format; this shell does not allocate storage.
    pub desired_pixel_format: PixelFormat,
    /// Namespace for the generational top-level surface identity.
    pub surface_namespace: SurfaceNamespace,
    /// X11/Wayland selection policy.
    pub backend: LinuxBackendPreference,
    /// Explicit resource limits.
    pub limits: LinuxShellLimits,
}

impl LinuxShellConfig {
    /// Creates Wild Buzzard's provider-neutral default window configuration.
    #[must_use]
    pub fn wild_buzzard_default(surface_namespace: SurfaceNamespace) -> Self {
        Self {
            title: "Wild Buzzard".to_owned(),
            application_id: "org.openresearchtools.WildBuzzard".to_owned(),
            initial_size: LogicalSize {
                width: 1_280.0,
                height: 800.0,
            },
            desired_pixel_format: PixelFormat::Rgba8Srgb,
            surface_namespace,
            backend: LinuxBackendPreference::Auto,
            limits: LinuxShellLimits::default(),
        }
    }

    pub(crate) fn validate(self) -> Result<Self, ConfigError> {
        validate_title(&self.title)?;
        validate_application_id(&self.application_id)?;
        self.limits.validate()?;

        // Reject an initial request which is already unsafe at scale 1. The
        // actual scale and physical size are checked again after creation.
        let width = ceil_to_u32(self.initial_size.width, "initial_width")?;
        let height = ceil_to_u32(self.initial_size.height, "initial_height")?;
        PhysicalSize::new(width, height)
            .map_err(|_| ConfigError::InitialSizeOutsideSurfaceBounds)?;

        Ok(self)
    }
}

/// Invalid caller configuration rejected before an event loop is started.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigError {
    /// The title was empty or exceeded its UTF-8 byte limit.
    InvalidTitleLength { actual: usize, maximum: usize },
    /// The title contained a control character.
    InvalidTitleCharacter { character: char },
    /// The application ID did not meet the documented Linux identifier form.
    InvalidApplicationId,
    /// The application ID exceeded its ASCII byte limit.
    ApplicationIdTooLong { actual: usize, maximum: usize },
    /// A caller-controlled capacity was outside its hard range.
    CapacityOutsideRange {
        field: &'static str,
        value: usize,
        minimum: usize,
        maximum: usize,
    },
    /// Initial logical dimensions could not be represented safely.
    InitialSizeOutsideSurfaceBounds,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTitleLength { actual, maximum } => write!(
                formatter,
                "window title contains {actual} bytes; expected 1..={maximum}"
            ),
            Self::InvalidTitleCharacter { character } => write!(
                formatter,
                "window title contains prohibited control character {character:?}"
            ),
            Self::InvalidApplicationId => formatter.write_str(
                "application ID must be a dotted ASCII identifier with at least two segments",
            ),
            Self::ApplicationIdTooLong { actual, maximum } => write!(
                formatter,
                "application ID contains {actual} bytes; maximum is {maximum}"
            ),
            Self::CapacityOutsideRange {
                field,
                value,
                minimum,
                maximum,
            } => write!(
                formatter,
                "{field}={value} is outside {minimum}..={maximum}"
            ),
            Self::InitialSizeOutsideSurfaceBounds => formatter.write_str(
                "initial logical size is outside the bounded surface contract at scale 1",
            ),
        }
    }
}

impl Error for ConfigError {}

fn validate_title(title: &str) -> Result<(), ConfigError> {
    if title.is_empty() || title.len() > MAX_TITLE_BYTES {
        return Err(ConfigError::InvalidTitleLength {
            actual: title.len(),
            maximum: MAX_TITLE_BYTES,
        });
    }
    if let Some(character) = title.chars().find(|character| character.is_control()) {
        return Err(ConfigError::InvalidTitleCharacter { character });
    }
    Ok(())
}

fn validate_application_id(application_id: &str) -> Result<(), ConfigError> {
    if application_id.len() > MAX_APPLICATION_ID_BYTES {
        return Err(ConfigError::ApplicationIdTooLong {
            actual: application_id.len(),
            maximum: MAX_APPLICATION_ID_BYTES,
        });
    }
    if !application_id.is_ascii() {
        return Err(ConfigError::InvalidApplicationId);
    }

    let mut segments = application_id.split('.');
    let Some(first) = segments.next() else {
        return Err(ConfigError::InvalidApplicationId);
    };
    if !valid_id_segment(first) {
        return Err(ConfigError::InvalidApplicationId);
    }
    let mut segment_count = 1_usize;
    for segment in segments {
        if !valid_id_segment(segment) {
            return Err(ConfigError::InvalidApplicationId);
        }
        segment_count = segment_count
            .checked_add(1)
            .ok_or(ConfigError::InvalidApplicationId)?;
    }
    if segment_count < 2 {
        return Err(ConfigError::InvalidApplicationId);
    }
    Ok(())
}

fn valid_id_segment(segment: &str) -> bool {
    let mut bytes = segment.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn validate_capacity(
    field: &'static str,
    value: usize,
    minimum: usize,
    maximum: usize,
) -> Result<(), ConfigError> {
    if (minimum..=maximum).contains(&value) {
        Ok(())
    } else {
        Err(ConfigError::CapacityOutsideRange {
            field,
            value,
            minimum,
            maximum,
        })
    }
}

fn ceil_to_u32(value: f64, _field: &'static str) -> Result<u32, ConfigError> {
    if !value.is_finite() || value < 0.0 || value.ceil() > f64::from(u32::MAX) {
        Err(ConfigError::InitialSizeOutsideSurfaceBounds)
    } else {
        Ok(value.ceil() as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ConfigError, LinuxShellConfig, LinuxShellLimits, MAX_APPLICATION_ID_BYTES,
        MAX_EVENT_CAPACITY,
    };
    use wild_buzzard_platform::{LogicalSize, SurfaceNamespace};

    fn config() -> LinuxShellConfig {
        LinuxShellConfig::wild_buzzard_default(SurfaceNamespace::new(41).unwrap())
    }

    #[test]
    fn defaults_use_wild_buzzard_identity_and_validate() {
        let validated = config().validate().unwrap();
        assert_eq!(validated.title, "Wild Buzzard");
        assert_eq!(
            validated.application_id,
            "org.openresearchtools.WildBuzzard"
        );
    }

    #[test]
    fn application_id_requires_bounded_dotted_ascii_segments() {
        for invalid in [
            "WildBuzzard",
            ".WildBuzzard",
            "org..WildBuzzard",
            "org.9WildBuzzard",
            "org.open research",
            "org.🦅",
        ] {
            let mut candidate = config();
            candidate.application_id = invalid.to_owned();
            assert_eq!(candidate.validate(), Err(ConfigError::InvalidApplicationId));
        }

        let mut candidate = config();
        candidate.application_id = "a".repeat(MAX_APPLICATION_ID_BYTES + 1);
        assert!(matches!(
            candidate.validate(),
            Err(ConfigError::ApplicationIdTooLong { .. })
        ));
    }

    #[test]
    fn capacities_and_initial_surface_are_hard_bounded() {
        let mut candidate = config();
        candidate.limits = LinuxShellLimits {
            event_capacity: MAX_EVENT_CAPACITY + 1,
            ..LinuxShellLimits::default()
        };
        assert!(matches!(
            candidate.validate(),
            Err(ConfigError::CapacityOutsideRange {
                field: "event_capacity",
                ..
            })
        ));

        let mut candidate = config();
        candidate.initial_size = LogicalSize {
            width: 40_000.0,
            height: 800.0,
        };
        assert_eq!(
            candidate.validate(),
            Err(ConfigError::InitialSizeOutsideSurfaceBounds)
        );
    }
}
