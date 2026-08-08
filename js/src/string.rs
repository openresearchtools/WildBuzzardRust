use std::error::Error;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

/// Maximum number of UTF-16 code units in one JavaScript string.
///
/// This matches Firefox ESR153's implementation limit. Keeping the value below
/// `u32::MAX` also makes every supported string length exactly representable by
/// the runtime's numeric and property-index machinery.
pub const MAX_STRING_LENGTH: u32 = (1 << 30) - 2;

/// An attempted JavaScript string length exceeded [`MAX_STRING_LENGTH`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StringLengthError {
    attempted: usize,
}

impl StringLengthError {
    const fn new(attempted: usize) -> Self {
        Self { attempted }
    }

    /// Returns the rejected length in UTF-16 code units.
    #[must_use]
    pub const fn attempted(self) -> usize {
        self.attempted
    }

    /// Returns the largest supported length in UTF-16 code units.
    #[must_use]
    pub const fn maximum() -> u32 {
        MAX_STRING_LENGTH
    }
}

impl fmt::Display for StringLengthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "JavaScript string length {} exceeds the maximum of {} UTF-16 code units",
            self.attempted, MAX_STRING_LENGTH
        )
    }
}

impl Error for StringLengthError {}

/// Owned immutable ECMAScript string contents.
///
/// ECMAScript strings are sequences of 16-bit code units, not necessarily
/// well-formed Unicode strings. This type therefore preserves lone surrogates,
/// embedded NULs, and every other `u16` value exactly. Equality, hashing, and
/// ordering all operate on code units without normalization or replacement.
#[derive(Clone)]
pub struct JsString {
    units: Arc<[u16]>,
}

impl JsString {
    /// Encodes a valid UTF-8 Rust string as ECMAScript UTF-16 code units.
    ///
    /// # Errors
    ///
    /// Returns [`StringLengthError`] if the encoded result exceeds
    /// [`MAX_STRING_LENGTH`].
    pub fn from_utf8(value: &str) -> Result<Self, StringLengthError> {
        let length = value.encode_utf16().count();
        validate_length(length)?;
        let mut units = Vec::with_capacity(length);
        units.extend(value.encode_utf16());
        Ok(Self {
            units: Arc::from(units),
        })
    }

    /// Copies an exact sequence of ECMAScript UTF-16 code units.
    ///
    /// Lone surrogates are retained unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`StringLengthError`] if `units` exceeds
    /// [`MAX_STRING_LENGTH`].
    pub fn from_code_units(units: &[u16]) -> Result<Self, StringLengthError> {
        validate_length(units.len())?;
        Ok(Self {
            units: Arc::from(units),
        })
    }

    pub(crate) fn from_runtime_utf8(value: &str) -> Self {
        Self::from_utf8(value).expect("runtime-generated string fits the JavaScript length limit")
    }

    pub(crate) fn from_single_code_unit(unit: u16) -> Self {
        Self {
            units: Arc::from([unit]),
        }
    }

    pub(crate) fn concat(&self, other: &Self) -> Result<Self, StringLengthError> {
        let length = checked_concatenated_length(self.units.len(), other.units.len())?;
        if self.is_empty() {
            return Ok(other.clone());
        }
        if other.is_empty() {
            return Ok(self.clone());
        }
        let mut units = Vec::with_capacity(length);
        units.extend_from_slice(&self.units);
        units.extend_from_slice(&other.units);
        Ok(Self {
            units: Arc::from(units),
        })
    }

    pub(crate) fn eq_utf8(&self, value: &str) -> bool {
        self.units.iter().copied().eq(value.encode_utf16())
    }

    /// Borrows the exact UTF-16 code units.
    #[must_use]
    pub fn as_code_units(&self) -> &[u16] {
        &self.units
    }

    /// Returns the ECMAScript length in UTF-16 code units.
    #[must_use]
    pub fn len_code_units(&self) -> usize {
        self.units.len()
    }

    /// Returns whether the string contains no code units.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }

    /// Returns whether every surrogate participates in a valid surrogate pair.
    #[must_use]
    pub fn is_well_formed(&self) -> bool {
        char::decode_utf16(self.units.iter().copied()).all(|unit| unit.is_ok())
    }

    /// Converts to a Rust UTF-8 string if the code units are well formed.
    ///
    /// # Errors
    ///
    /// Returns [`std::string::FromUtf16Error`] when an unpaired surrogate is
    /// present.
    pub fn to_utf8(&self) -> Result<String, std::string::FromUtf16Error> {
        String::from_utf16(&self.units)
    }

    /// Converts to UTF-8, replacing each unpaired surrogate with U+FFFD.
    ///
    /// This conversion is intended for diagnostics and explicit USVString-like
    /// boundaries. It must not be used for JavaScript equality or property
    /// lookup.
    #[must_use]
    pub fn to_utf8_lossy(&self) -> String {
        String::from_utf16_lossy(&self.units)
    }
}

impl Default for JsString {
    fn default() -> Self {
        Self {
            units: Arc::from([]),
        }
    }
}

impl fmt::Debug for JsString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("JsString")
            .field(&self.units)
            .finish()
    }
}

impl PartialEq for JsString {
    fn eq(&self, other: &Self) -> bool {
        self.units == other.units
    }
}

impl Eq for JsString {}

impl PartialOrd for JsString {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for JsString {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.units.cmp(&other.units)
    }
}

impl Hash for JsString {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.units.hash(state);
    }
}

impl AsRef<[u16]> for JsString {
    fn as_ref(&self) -> &[u16] {
        self.as_code_units()
    }
}

fn validate_length(length: usize) -> Result<usize, StringLengthError> {
    if length > MAX_STRING_LENGTH as usize {
        Err(StringLengthError::new(length))
    } else {
        Ok(length)
    }
}

fn checked_concatenated_length(left: usize, right: usize) -> Result<usize, StringLengthError> {
    let length = left
        .checked_add(right)
        .ok_or_else(|| StringLengthError::new(usize::MAX))?;
    validate_length(length)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::{JsString, MAX_STRING_LENGTH, StringLengthError, checked_concatenated_length};

    #[test]
    fn preserves_exact_code_units_and_distinguishes_replacement() {
        let lone = JsString::from_code_units(&[0xd800, 0, 0xdc00]).unwrap();
        assert_eq!(lone.as_code_units(), &[0xd800, 0, 0xdc00]);
        assert!(!lone.is_well_formed());
        assert!(lone.to_utf8().is_err());
        assert_eq!(lone.to_utf8_lossy(), "\u{fffd}\0\u{fffd}");
        assert_ne!(lone, JsString::from_utf8("\u{fffd}\0\u{fffd}").unwrap());
    }

    #[test]
    fn utf8_scalar_and_explicit_surrogate_pair_are_equal_keys() {
        let scalar = JsString::from_utf8("\u{10000}").unwrap();
        let pair = JsString::from_code_units(&[0xd800, 0xdc00]).unwrap();
        assert_eq!(scalar, pair);

        let mut properties = HashMap::new();
        properties.insert(scalar, 42);
        assert_eq!(properties.get(&pair), Some(&42));
    }

    #[test]
    fn ordering_is_lexicographic_by_code_unit() {
        let supplementary = JsString::from_utf8("\u{10000}").unwrap();
        let bmp = JsString::from_utf8("\u{ffff}").unwrap();
        assert!(supplementary < bmp);
        assert!(
            JsString::from_code_units(&[0xd800]).unwrap()
                < JsString::from_code_units(&[0xdc00]).unwrap()
        );
    }

    #[test]
    fn concatenation_preserves_cross_boundary_surrogate_pairs() {
        let lead = JsString::from_code_units(&[0xd83d]).unwrap();
        let trail = JsString::from_code_units(&[0xdca9]).unwrap();
        let joined = lead.concat(&trail).unwrap();
        assert_eq!(joined.as_code_units(), &[0xd83d, 0xdca9]);
        assert!(joined.is_well_formed());
        assert_eq!(joined.to_utf8().unwrap(), "💩");
    }

    #[test]
    fn maximum_length_arithmetic_is_checked_without_large_allocations() {
        let maximum = MAX_STRING_LENGTH as usize;
        assert_eq!(checked_concatenated_length(maximum - 1, 1), Ok(maximum));
        let error = checked_concatenated_length(maximum, 1).unwrap_err();
        assert_eq!(error.attempted(), maximum + 1);
        assert_eq!(StringLengthError::maximum(), MAX_STRING_LENGTH);
    }
}
