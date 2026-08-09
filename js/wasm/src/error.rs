use std::fmt;

/// The kind of opaque identity involved in an adapter error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityKind {
    Module,
    Store,
    Instance,
}

impl fmt::Display for IdentityKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Module => formatter.write_str("module"),
            Self::Store => formatter.write_str("store"),
            Self::Instance => formatter.write_str("instance"),
        }
    }
}

/// Typed, browser-facing failures from the capability-free Wasm adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum WasmError {
    InvalidLimit {
        name: &'static str,
        reason: &'static str,
    },
    EngineCreation {
        detail: String,
    },
    ModuleTooLarge {
        actual: usize,
        maximum: usize,
    },
    TruncatedBinaryHeader {
        actual: usize,
    },
    InvalidBinaryMagic,
    InvalidBinaryVersion {
        found: [u8; 4],
    },
    ValidationFailed {
        detail: String,
    },
    CompilationFailed {
        detail: String,
    },
    ImportsForbidden {
        count: usize,
        first_module: String,
        first_name: String,
    },
    CapacityExceeded {
        kind: IdentityKind,
        maximum: usize,
    },
    ForeignIdentity {
        kind: IdentityKind,
    },
    StaleIdentity {
        kind: IdentityKind,
    },
    WrongStoreAssociation,
    WrongModuleAssociation,
    ResourceInUse {
        kind: IdentityKind,
        dependents: usize,
    },
    ExportNameTooLong {
        actual: usize,
        maximum: usize,
    },
    ExportNotFound {
        name: String,
    },
    ExportNotFunction {
        name: String,
    },
    UnsupportedSignature {
        parameters: usize,
        results: usize,
    },
    WrongArgumentCount {
        expected: usize,
        actual: usize,
    },
    FuelExhausted,
    Interrupted,
    ExecutionTrap {
        detail: String,
    },
    InstantiationFailed {
        detail: String,
    },
    RuntimeClosed,
    IdentitySpaceExhausted,
    InterruptSequenceExhausted,
    HostAllocationFailed,
    InternalInvariant {
        detail: &'static str,
    },
}

impl fmt::Display for WasmError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimit { name, reason } => {
                write!(formatter, "invalid WebAssembly limit `{name}`: {reason}")
            }
            Self::EngineCreation { detail } => {
                write!(
                    formatter,
                    "could not create the WebAssembly engine: {detail}"
                )
            }
            Self::ModuleTooLarge { actual, maximum } => write!(
                formatter,
                "WebAssembly module is {actual} bytes; policy allows at most {maximum}"
            ),
            Self::TruncatedBinaryHeader { actual } => write!(
                formatter,
                "WebAssembly binary header is truncated at {actual} bytes"
            ),
            Self::InvalidBinaryMagic => {
                formatter.write_str("input is not a core WebAssembly binary")
            }
            Self::InvalidBinaryVersion { found } => write!(
                formatter,
                "unsupported core WebAssembly binary version {:02x?}",
                found
            ),
            Self::ValidationFailed { detail } => {
                write!(formatter, "WebAssembly validation failed: {detail}")
            }
            Self::CompilationFailed { detail } => {
                write!(formatter, "WebAssembly compilation failed: {detail}")
            }
            Self::ImportsForbidden {
                count,
                first_module,
                first_name,
            } => write!(
                formatter,
                "WebAssembly imports are forbidden ({count} imports; first is {first_module}.{first_name})"
            ),
            Self::CapacityExceeded { kind, maximum } => {
                write!(formatter, "{kind} capacity of {maximum} has been reached")
            }
            Self::ForeignIdentity { kind } => {
                write!(
                    formatter,
                    "{kind} identity belongs to another process owner"
                )
            }
            Self::StaleIdentity { kind } => write!(formatter, "{kind} identity is stale"),
            Self::WrongStoreAssociation => {
                formatter.write_str("instance is not associated with the supplied store")
            }
            Self::WrongModuleAssociation => {
                formatter.write_str("instance is not associated with the supplied module")
            }
            Self::ResourceInUse { kind, dependents } => {
                write!(formatter, "{kind} still has {dependents} live dependents")
            }
            Self::ExportNameTooLong { actual, maximum } => write!(
                formatter,
                "export name is {actual} bytes; policy allows at most {maximum}"
            ),
            Self::ExportNotFound { name } => write!(formatter, "export `{name}` was not found"),
            Self::ExportNotFunction { name } => {
                write!(formatter, "export `{name}` is not a function")
            }
            Self::UnsupportedSignature {
                parameters,
                results,
            } => write!(
                formatter,
                "function signature is outside the i32-only gate ({parameters} parameters, {results} results)"
            ),
            Self::WrongArgumentCount { expected, actual } => write!(
                formatter,
                "function expected {expected} arguments but received {actual}"
            ),
            Self::FuelExhausted => formatter.write_str("WebAssembly fuel was exhausted"),
            Self::Interrupted => formatter.write_str("WebAssembly execution was interrupted"),
            Self::ExecutionTrap { detail } => write!(formatter, "WebAssembly trapped: {detail}"),
            Self::InstantiationFailed { detail } => {
                write!(formatter, "WebAssembly instantiation failed: {detail}")
            }
            Self::RuntimeClosed => formatter.write_str("WebAssembly process owner is closed"),
            Self::IdentitySpaceExhausted => {
                formatter.write_str("WebAssembly identity space is exhausted")
            }
            Self::InterruptSequenceExhausted => {
                formatter.write_str("WebAssembly interrupt sequence is exhausted")
            }
            Self::HostAllocationFailed => {
                formatter.write_str("host allocation for WebAssembly bookkeeping failed")
            }
            Self::InternalInvariant { detail } => {
                write!(formatter, "WebAssembly adapter invariant failed: {detail}")
            }
        }
    }
}

impl std::error::Error for WasmError {}
