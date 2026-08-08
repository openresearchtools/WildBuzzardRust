//! Wild Buzzard's Rust-native JavaScript runtime nucleus.
//!
//! The embedding boundary deliberately exposes rooted values rather than heap
//! addresses. The heap is a non-moving stop-the-world tracing collector whose
//! private typed handles use checked generations and permanently retired slots;
//! collection does not change the public [`Engine`], [`Realm`], [`Context`], or
//! [`RootedValue`] lifecycle boundary.

mod ast;
mod error;
mod heap;
mod lexer;
mod parser;
mod runtime;
mod source;

pub use error::{DiagnosticLocation, ErrorKind, JsError, JsResult, StackFrame};
pub use runtime::{
    ArenaStatistics, CollectionError, CollectionErrorKind, CollectionReport, CompiledScript,
    Context, Engine, EngineOptions, ExecutionLimits, HeapArenaStatistics, HeapStatistics,
    HostFunction, Job, JobRunError, Realm, RealmId, RealmOptions, ReclaimedStatistics, RootedValue,
    ValueSnapshot, ValueType,
};
pub use source::{SourceLocation, SourceSpan, SourceText};
