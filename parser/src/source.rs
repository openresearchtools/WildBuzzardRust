/// One-based line/column and zero-based UTF-8 byte offset in the source.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SourcePosition {
    pub byte: usize,
    pub line: usize,
    pub column: usize,
}

impl Default for SourcePosition {
    fn default() -> Self {
        Self {
            byte: 0,
            line: 1,
            column: 1,
        }
    }
}

impl SourcePosition {
    /// Advances using HTML newline rules: CRLF is one newline and a lone CR is
    /// also a newline. Columns count Unicode scalar values, not UTF-8 bytes.
    pub fn advanced_by(mut self, source: &str) -> Self {
        let mut chars = source.chars().peekable();
        while let Some(character) = chars.next() {
            self.byte += character.len_utf8();
            match character {
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        let newline = chars.next().expect("peeked newline");
                        self.byte += newline.len_utf8();
                    }
                    self.line += 1;
                    self.column = 1;
                }
                '\n' => {
                    self.line += 1;
                    self.column = 1;
                }
                _ => self.column += 1,
            }
        }
        self
    }
}

/// Half-open source range.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SourceSpan {
    pub start: SourcePosition,
    pub end: SourcePosition,
}

impl SourceSpan {
    pub fn new(start: SourcePosition, source: &str) -> Self {
        Self {
            start,
            end: start.advanced_by(source),
        }
    }
}
