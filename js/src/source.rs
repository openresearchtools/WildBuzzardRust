use std::sync::Arc;

/// A one-based line and column paired with a zero-based UTF-8 byte offset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceLocation {
    /// One-based source line.
    pub line: u32,
    /// One-based Unicode-scalar column.
    pub column: u32,
    /// Zero-based UTF-8 byte offset.
    pub byte_offset: usize,
}

impl SourceLocation {
    pub(crate) const fn start() -> Self {
        Self {
            line: 1,
            column: 1,
            byte_offset: 0,
        }
    }
}

/// Half-open source range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceSpan {
    /// First source position covered by the range.
    pub start: SourceLocation,
    /// First source position after the range.
    pub end: SourceLocation,
}

impl SourceSpan {
    pub(crate) const fn new(start: SourceLocation, end: SourceLocation) -> Self {
        Self { start, end }
    }

    pub(crate) const fn join(self, other: Self) -> Self {
        Self::new(self.start, other.end)
    }
}

/// Owned script source and its diagnostic name.
#[derive(Clone, Debug)]
pub struct SourceText {
    name: Arc<str>,
    text: Arc<str>,
}

impl SourceText {
    /// Creates source text. `name` is used in diagnostics and stack frames.
    pub fn new(name: impl Into<Arc<str>>, text: impl Into<Arc<str>>) -> Self {
        Self {
            name: name.into(),
            text: text.into(),
        }
    }

    /// Returns the source's diagnostic name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the JavaScript source text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn name_arc(&self) -> Arc<str> {
        Arc::clone(&self.name)
    }
}
