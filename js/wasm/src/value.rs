/// A scalar WebAssembly value type admitted by the browser-owned call boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WasmScalarType {
    /// A 32-bit integer.
    I32,
    /// A 64-bit integer.
    I64,
    /// A 32-bit IEEE-754 floating-point value.
    F32,
    /// A 64-bit IEEE-754 floating-point value.
    F64,
}

/// A scalar value which can cross the browser-owned WebAssembly call boundary.
///
/// Floating-point variants contain their exact IEEE-754 bit patterns. This preserves NaN
/// payloads and the distinction between positive and negative zero without host-language
/// floating-point conversion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WasmScalarValue {
    /// A 32-bit integer.
    I32(i32),
    /// A 64-bit integer.
    I64(i64),
    /// The exact bits of a 32-bit IEEE-754 floating-point value.
    F32Bits(u32),
    /// The exact bits of a 64-bit IEEE-754 floating-point value.
    F64Bits(u64),
}

impl WasmScalarValue {
    /// Returns the WebAssembly scalar type represented by this value.
    pub const fn value_type(self) -> WasmScalarType {
        match self {
            Self::I32(_) => WasmScalarType::I32,
            Self::I64(_) => WasmScalarType::I64,
            Self::F32Bits(_) => WasmScalarType::F32,
            Self::F64Bits(_) => WasmScalarType::F64,
        }
    }
}
