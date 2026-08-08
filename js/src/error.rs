use std::error::Error;
use std::fmt;

use crate::runtime::RootedValue;
use crate::source::SourceSpan;

/// JavaScript diagnostic category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ErrorKind {
    /// Source text is not valid in the implemented grammar.
    SyntaxError,
    /// A binding or value could not be resolved.
    ReferenceError,
    /// An operation received a value of the wrong type.
    TypeError,
    /// A numeric or recursion limit was exceeded.
    RangeError,
    /// The embedding's deterministic execution budget was exhausted.
    ResourceLimit,
    /// Script explicitly threw a value that was not an engine-created error.
    Exception,
    /// An engine invariant failed without memory unsafety.
    InternalError,
}

impl ErrorKind {
    /// ECMAScript-style display name for the category.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::SyntaxError => "SyntaxError",
            Self::ReferenceError => "ReferenceError",
            Self::TypeError => "TypeError",
            Self::RangeError => "RangeError",
            Self::ResourceLimit => "ResourceLimitError",
            Self::Exception => "Exception",
            Self::InternalError => "InternalError",
        }
    }
}

/// Source name and exact span associated with a diagnostic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticLocation {
    /// Embedding-provided source name.
    pub source_name: String,
    /// Half-open span within the source.
    pub span: SourceSpan,
}

/// One captured JavaScript call frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StackFrame {
    /// Function display name, or `<anonymous>`.
    pub function_name: String,
    /// Call site, when the call originated in parsed source.
    pub call_site: Option<DiagnosticLocation>,
}

/// Structured error returned to an embedder.
#[derive(Clone, Debug)]
pub struct JsError(Box<JsErrorInner>);

#[derive(Clone, Debug)]
struct JsErrorInner {
    kind: ErrorKind,
    message: String,
    location: Option<DiagnosticLocation>,
    stack: Vec<StackFrame>,
    exception: Option<RootedValue>,
}

impl JsError {
    /// Creates a host-originated error without a source location.
    ///
    /// Parsed and interpreted code normally produces errors with locations via
    /// [`Context::evaluate`](crate::Context::evaluate).
    #[must_use]
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self(Box::new(JsErrorInner {
            kind,
            message: message.into(),
            location: None,
            stack: Vec::new(),
            exception: None,
        }))
    }

    pub(crate) fn located(
        kind: ErrorKind,
        message: impl Into<String>,
        location: DiagnosticLocation,
        stack: Vec<StackFrame>,
    ) -> Self {
        Self(Box::new(JsErrorInner {
            kind,
            message: message.into(),
            location: Some(location),
            stack,
            exception: None,
        }))
    }

    pub(crate) fn thrown(
        message: impl Into<String>,
        location: DiagnosticLocation,
        stack: Vec<StackFrame>,
        exception: RootedValue,
    ) -> Self {
        Self(Box::new(JsErrorInner {
            kind: ErrorKind::Exception,
            message: message.into(),
            location: Some(location),
            stack,
            exception: Some(exception),
        }))
    }

    /// Returns the diagnostic category.
    #[must_use]
    pub const fn kind(&self) -> ErrorKind {
        self.0.kind
    }

    /// Returns the human-readable diagnostic message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.0.message
    }

    /// Returns the originating source span, if available.
    #[must_use]
    pub const fn location(&self) -> Option<&DiagnosticLocation> {
        self.0.location.as_ref()
    }

    /// Returns the captured JavaScript call frames, innermost first.
    #[must_use]
    pub fn stack(&self) -> &[StackFrame] {
        &self.0.stack
    }

    /// Returns the rooted value from an explicit `throw`, when present.
    #[must_use]
    pub const fn exception_value(&self) -> Option<&RootedValue> {
        self.0.exception.as_ref()
    }
}

impl fmt::Display for JsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.0.kind.name(), self.0.message)?;
        if let Some(location) = &self.0.location {
            write!(
                formatter,
                " at {}:{}:{}",
                location.source_name, location.span.start.line, location.span.start.column
            )?;
        }
        Ok(())
    }
}

impl Error for JsError {}

/// Result type used by the embedding API.
pub type JsResult<T> = Result<T, JsError>;

#[derive(Clone, Debug)]
pub(crate) struct SyntaxIssue {
    pub message: String,
    pub span: SourceSpan,
}

impl SyntaxIssue {
    pub(crate) fn new(message: impl Into<String>, span: SourceSpan) -> Self {
        Self {
            message: message.into(),
            span,
        }
    }
}
