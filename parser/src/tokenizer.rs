use std::fmt;

use crate::source::{SourcePosition, SourceSpan};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParsePhase {
    Tokenizer,
    TreeBuilder,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ParseErrorCode {
    UnexpectedNullCharacter,
    InvalidFirstCharacterOfTagName,
    EofInTag,
    EofInComment,
    EofInScript,
    AbruptClosingOfEmptyComment,
    IncorrectlyOpenedComment,
    MissingDoctypeName,
    InvalidDoctype,
    UnexpectedCharacterAfterDoctypeName,
    MissingEndTagName,
    UnexpectedCharacterAfterEndTagName,
    UnexpectedEqualsSignBeforeAttributeName,
    MissingAttributeValue,
    UnexpectedCharacterInUnquotedAttributeValue,
    DuplicateAttribute,
    EndTagWithAttributes,
    EndTagWithTrailingSolidus,
    NonVoidHtmlElementStartTagWithTrailingSolidus,
    UnknownNamedCharacterReference,
    AbsenceOfDigitsInNumericCharacterReference,
    MissingSemicolonAfterCharacterReference,
    NullCharacterReference,
    CharacterReferenceOutsideUnicodeRange,
    SurrogateCharacterReference,
    ControlCharacterReference,
    TokenLimitExceeded,
    AttributeLimitExceeded,
    TreeDepthLimitExceeded,
    UnexpectedDoctype,
    UnexpectedStartTag,
    UnexpectedEndTag,
    MismatchedEndTag,
    UnexpectedCharactersBeforeDocumentElement,
    UnexpectedCharactersInHead,
    InternalDomInvariant,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParseError {
    pub phase: ParsePhase,
    pub code: ParseErrorCode,
    pub position: SourcePosition,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpannedAttribute {
    pub name: String,
    pub value: String,
    pub span: SourceSpan,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Token {
    Doctype {
        name: String,
        public_id: String,
        system_id: String,
        force_quirks: bool,
    },
    StartTag {
        name: String,
        attributes: Vec<SpannedAttribute>,
        self_closing: bool,
    },
    EndTag {
        name: String,
    },
    Comment(String),
    Character(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpannedToken {
    pub token: Token,
    pub span: SourceSpan,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TokenizerLimits {
    /// Maximum bytes retained for one incomplete tag, comment, or doctype.
    pub max_token_bytes: usize,
    /// Maximum attributes emitted by one start tag. Later attributes are dropped.
    pub max_attributes_per_tag: usize,
    /// Maximum open-element stack depth used by `HtmlParser`.
    pub max_tree_depth: usize,
}

impl Default for TokenizerLimits {
    fn default() -> Self {
        Self {
            max_token_bytes: 1024 * 1024,
            max_attributes_per_tag: 4096,
            max_tree_depth: 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TokenizerStateError {
    AlreadyFinished,
}

impl fmt::Display for TokenizerStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HTML tokenizer has already reached end of input")
    }
}

impl std::error::Error for TokenizerStateError {}

#[derive(Clone, Debug)]
enum TextMode {
    RawText { end_tag: String },
    RcData { end_tag: String },
    PlainText,
}

/// Incremental tokenizer. Input can be split at any UTF-8 string boundary.
#[derive(Debug)]
pub struct Tokenizer {
    pending: String,
    position: SourcePosition,
    errors: Vec<ParseError>,
    limits: TokenizerLimits,
    text_mode: Option<TextMode>,
    finished: bool,
}

impl Default for Tokenizer {
    fn default() -> Self {
        Self::new(TokenizerLimits::default())
    }
}

impl Tokenizer {
    pub fn new(limits: TokenizerLimits) -> Self {
        Self {
            pending: String::new(),
            position: SourcePosition::default(),
            errors: Vec::new(),
            limits,
            text_mode: None,
            finished: false,
        }
    }

    pub fn feed(&mut self, input: &str) -> Result<Vec<SpannedToken>, TokenizerStateError> {
        if self.finished {
            return Err(TokenizerStateError::AlreadyFinished);
        }
        self.pending.push_str(input);
        Ok(self.drain(false))
    }

    pub fn finish(&mut self) -> Result<Vec<SpannedToken>, TokenizerStateError> {
        if self.finished {
            return Err(TokenizerStateError::AlreadyFinished);
        }
        self.finished = true;
        Ok(self.drain(true))
    }

    pub fn take_errors(&mut self) -> Vec<ParseError> {
        std::mem::take(&mut self.errors)
    }

    pub fn position(&self) -> SourcePosition {
        self.position
    }

    fn drain(&mut self, eof: bool) -> Vec<SpannedToken> {
        let mut output = Vec::new();
        loop {
            if self.pending.is_empty() {
                break;
            }
            let progressed = if let Some(mode) = self.text_mode.clone() {
                self.consume_text_mode(mode, eof, &mut output)
            } else {
                self.consume_data(eof, &mut output)
            };
            if !progressed {
                break;
            }
        }
        output
    }

    fn consume_text_mode(
        &mut self,
        mode: TextMode,
        eof: bool,
        output: &mut Vec<SpannedToken>,
    ) -> bool {
        if matches!(mode, TextMode::PlainText) {
            let length = if eof {
                self.pending.len()
            } else {
                safe_stream_text_prefix(&self.pending, false, true)
            };
            if length == 0 {
                return false;
            }
            self.emit_characters(length, false, output);
            return true;
        }

        let (end_tag, decode_entities) = match &mode {
            TextMode::RawText { end_tag } => (end_tag.as_str(), false),
            TextMode::RcData { end_tag } => (end_tag.as_str(), true),
            TextMode::PlainText => unreachable!(),
        };
        if let Some(index) = find_appropriate_end_tag(&self.pending, end_tag) {
            if index == 0 {
                self.text_mode = None;
                return true;
            }
            self.emit_characters(index, decode_entities, output);
            return true;
        }
        if eof {
            let length = self.pending.len();
            self.emit_characters(length, decode_entities, output);
            return true;
        }

        let retain = end_tag.len().saturating_add(4);
        if self.pending.len() <= retain {
            return false;
        }
        let mut length = floor_char_boundary(&self.pending, self.pending.len() - retain);
        if decode_entities {
            length = length.min(safe_stream_text_prefix(&self.pending[..length], true, true));
        }
        if self.pending[..length].ends_with('\r') {
            length -= 1;
        }
        if length == 0 {
            return false;
        }
        self.emit_characters(length, decode_entities, output);
        true
    }

    fn consume_data(&mut self, eof: bool, output: &mut Vec<SpannedToken>) -> bool {
        if !self.pending.starts_with('<') {
            let less_than = self.pending.find('<');
            let length = match less_than {
                Some(index) => index,
                None if eof => self.pending.len(),
                None => safe_stream_text_prefix(&self.pending, true, true),
            };
            if length == 0 {
                return false;
            }
            self.emit_characters(length, true, output);
            return true;
        }

        if self.pending.len() == 1 && !eof {
            return false;
        }
        if is_prefix_ascii_case_insensitive(&self.pending, "<!--") && self.pending.len() < 4 && !eof
        {
            return false;
        }
        if self.pending.starts_with("<!--") {
            return self.consume_comment(eof, output);
        }
        if is_prefix_ascii_case_insensitive(&self.pending, "<!doctype")
            && self.pending.len() < "<!doctype".len()
            && !eof
        {
            return false;
        }
        if starts_ascii_case_insensitive(&self.pending, "<!doctype") {
            return self.consume_doctype(eof, output);
        }
        if self.pending.starts_with("<!") || self.pending.starts_with("<?") {
            return self.consume_bogus_comment(eof, output);
        }
        if self.pending.starts_with("</") {
            return self.consume_end_tag(eof, output);
        }
        let after_less_than = self.pending[1..].chars().next();
        if after_less_than.is_some_and(|character| character.is_ascii_alphabetic()) {
            return self.consume_start_tag(eof, output);
        }
        if after_less_than.is_none() && !eof {
            return false;
        }

        self.push_error(
            ParseErrorCode::InvalidFirstCharacterOfTagName,
            self.position,
        );
        self.emit_characters(1, false, output);
        true
    }

    fn consume_comment(&mut self, eof: bool, output: &mut Vec<SpannedToken>) -> bool {
        let end = self.pending[4..].find("-->").map(|index| index + 4);
        let (content_end, consumed, eof_error) = match end {
            Some(index) => (index, index + 3, false),
            None if !eof && self.pending.len() <= self.limits.max_token_bytes => return false,
            None if !eof => {
                self.push_error(ParseErrorCode::TokenLimitExceeded, self.position);
                self.emit_character_literal(1, output);
                return true;
            }
            None => (self.pending.len(), self.pending.len(), true),
        };
        if eof_error {
            self.push_error(ParseErrorCode::EofInComment, self.position);
        }
        let source = self.pending[..consumed].to_owned();
        let content = normalize_newlines(&self.pending[4..content_end]);
        let span = SourceSpan::new(self.position, &source);
        self.consume_prefix(consumed);
        output.push(SpannedToken {
            token: Token::Comment(content),
            span,
        });
        true
    }

    fn consume_bogus_comment(&mut self, eof: bool, output: &mut Vec<SpannedToken>) -> bool {
        let end = self.pending.find('>');
        let consumed = match end {
            Some(index) => index + 1,
            None if !eof && self.pending.len() <= self.limits.max_token_bytes => return false,
            None if !eof => {
                self.push_error(ParseErrorCode::TokenLimitExceeded, self.position);
                self.emit_character_literal(1, output);
                return true;
            }
            None => self.pending.len(),
        };
        self.push_error(ParseErrorCode::IncorrectlyOpenedComment, self.position);
        let source = self.pending[..consumed].to_owned();
        let content_start = 2.min(consumed);
        let content_end = consumed.saturating_sub(usize::from(source.ends_with('>')));
        let content = normalize_newlines(&source[content_start..content_end]);
        let span = SourceSpan::new(self.position, &source);
        self.consume_prefix(consumed);
        output.push(SpannedToken {
            token: Token::Comment(content),
            span,
        });
        true
    }

    fn consume_doctype(&mut self, eof: bool, output: &mut Vec<SpannedToken>) -> bool {
        let Some(consumed) = self.complete_tag_length(eof) else {
            return false;
        };
        if consumed == 0 {
            return true;
        }
        let source = self.pending[..consumed].to_owned();
        let (name, public_id, system_id, force_quirks) = self.parse_doctype(&source);
        let span = SourceSpan::new(self.position, &source);
        self.consume_prefix(consumed);
        output.push(SpannedToken {
            token: Token::Doctype {
                name,
                public_id,
                system_id,
                force_quirks,
            },
            span,
        });
        true
    }

    fn parse_doctype(&mut self, source: &str) -> (String, String, String, bool) {
        let content_end = source.rfind('>').unwrap_or(source.len());
        let mut cursor = "<!doctype".len().min(content_end);
        let bytes = source.as_bytes();
        if cursor < content_end && !bytes[cursor].is_ascii_whitespace() {
            self.push_relative_error(ParseErrorCode::InvalidDoctype, source, cursor);
        }
        skip_ascii_whitespace(bytes, &mut cursor, content_end);
        let name_start = cursor;
        while cursor < content_end && !bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        let name = source[name_start..cursor].to_ascii_lowercase();
        if name.is_empty() {
            self.push_relative_error(ParseErrorCode::MissingDoctypeName, source, cursor);
            return (name, String::new(), String::new(), true);
        }
        let mut force_quirks = !name.eq_ignore_ascii_case("html");
        skip_ascii_whitespace(bytes, &mut cursor, content_end);
        if cursor == content_end {
            return (name, String::new(), String::new(), force_quirks);
        }

        let remainder = &source[cursor..content_end];
        if starts_ascii_case_insensitive(remainder, "public") {
            cursor += "public".len();
            let public_id = parse_quoted_identifier(source, bytes, &mut cursor, content_end);
            let system_id = parse_quoted_identifier(source, bytes, &mut cursor, content_end);
            if public_id.is_none() {
                self.push_relative_error(ParseErrorCode::InvalidDoctype, source, cursor);
                force_quirks = true;
            }
            skip_ascii_whitespace(bytes, &mut cursor, content_end);
            if cursor != content_end {
                self.push_relative_error(
                    ParseErrorCode::UnexpectedCharacterAfterDoctypeName,
                    source,
                    cursor,
                );
                force_quirks = true;
            }
            (
                name,
                public_id.unwrap_or_default(),
                system_id.unwrap_or_default(),
                force_quirks,
            )
        } else if starts_ascii_case_insensitive(remainder, "system") {
            cursor += "system".len();
            let system_id = parse_quoted_identifier(source, bytes, &mut cursor, content_end);
            if system_id.is_none() {
                self.push_relative_error(ParseErrorCode::InvalidDoctype, source, cursor);
                force_quirks = true;
            }
            skip_ascii_whitespace(bytes, &mut cursor, content_end);
            if cursor != content_end {
                self.push_relative_error(
                    ParseErrorCode::UnexpectedCharacterAfterDoctypeName,
                    source,
                    cursor,
                );
                force_quirks = true;
            }
            (
                name,
                String::new(),
                system_id.unwrap_or_default(),
                force_quirks,
            )
        } else {
            self.push_relative_error(
                ParseErrorCode::UnexpectedCharacterAfterDoctypeName,
                source,
                cursor,
            );
            (name, String::new(), String::new(), true)
        }
    }

    fn consume_end_tag(&mut self, eof: bool, output: &mut Vec<SpannedToken>) -> bool {
        let Some(consumed) = self.complete_tag_length(eof) else {
            return false;
        };
        if consumed == 0 {
            return true;
        }
        let source = self.pending[..consumed].to_owned();
        let content_end = source.rfind('>').unwrap_or(source.len());
        let bytes = source.as_bytes();
        let mut cursor = 2;
        skip_ascii_whitespace(bytes, &mut cursor, content_end);
        let name_start = cursor;
        while cursor < content_end && !bytes[cursor].is_ascii_whitespace() && bytes[cursor] != b'/'
        {
            cursor += 1;
        }
        let name = source[name_start..cursor].to_ascii_lowercase();
        if name.is_empty() {
            self.push_relative_error(ParseErrorCode::MissingEndTagName, &source, cursor);
        }
        skip_ascii_whitespace(bytes, &mut cursor, content_end);
        if cursor < content_end {
            if bytes[cursor] == b'/' {
                self.push_relative_error(
                    ParseErrorCode::EndTagWithTrailingSolidus,
                    &source,
                    cursor,
                );
            } else {
                self.push_relative_error(ParseErrorCode::EndTagWithAttributes, &source, cursor);
            }
            self.push_relative_error(
                ParseErrorCode::UnexpectedCharacterAfterEndTagName,
                &source,
                cursor,
            );
        }
        let span = SourceSpan::new(self.position, &source);
        self.consume_prefix(consumed);
        if !name.is_empty() {
            output.push(SpannedToken {
                token: Token::EndTag { name },
                span,
            });
        }
        true
    }

    fn consume_start_tag(&mut self, eof: bool, output: &mut Vec<SpannedToken>) -> bool {
        let Some(consumed) = self.complete_tag_length(eof) else {
            return false;
        };
        if consumed == 0 {
            return true;
        }
        let source = self.pending[..consumed].to_owned();
        let content_end = source.rfind('>').unwrap_or(source.len());
        let bytes = source.as_bytes();
        let mut cursor = 1;
        let name_start = cursor;
        while cursor < content_end && !bytes[cursor].is_ascii_whitespace() && bytes[cursor] != b'/'
        {
            cursor += 1;
        }
        let name = source[name_start..cursor].to_ascii_lowercase();
        let mut attributes = Vec::new();
        let mut self_closing = false;

        while cursor < content_end {
            skip_ascii_whitespace(bytes, &mut cursor, content_end);
            if cursor >= content_end {
                break;
            }
            if bytes[cursor] == b'/' {
                self_closing = true;
                cursor += 1;
                skip_ascii_whitespace(bytes, &mut cursor, content_end);
                if cursor < content_end {
                    self.push_relative_error(
                        ParseErrorCode::UnexpectedCharacterInUnquotedAttributeValue,
                        &source,
                        cursor,
                    );
                }
                break;
            }
            if bytes[cursor] == b'=' {
                self.push_relative_error(
                    ParseErrorCode::UnexpectedEqualsSignBeforeAttributeName,
                    &source,
                    cursor,
                );
            }
            let attribute_start = cursor;
            while cursor < content_end
                && !bytes[cursor].is_ascii_whitespace()
                && !matches!(bytes[cursor], b'=' | b'/')
            {
                cursor += 1;
            }
            let attribute_name = source[attribute_start..cursor].to_ascii_lowercase();
            if attribute_name.is_empty() {
                cursor += 1;
                continue;
            }
            skip_ascii_whitespace(bytes, &mut cursor, content_end);
            let mut value = String::new();
            if cursor < content_end && bytes[cursor] == b'=' {
                cursor += 1;
                skip_ascii_whitespace(bytes, &mut cursor, content_end);
                if cursor >= content_end {
                    self.push_relative_error(
                        ParseErrorCode::MissingAttributeValue,
                        &source,
                        cursor,
                    );
                } else if matches!(bytes[cursor], b'\'' | b'"') {
                    let quote = bytes[cursor];
                    cursor += 1;
                    let value_start = cursor;
                    while cursor < content_end && bytes[cursor] != quote {
                        cursor += 1;
                    }
                    value = self.decode_references(
                        &source[value_start..cursor],
                        self.position.advanced_by(&source[..value_start]),
                        true,
                    );
                    if cursor < content_end {
                        cursor += 1;
                    } else {
                        self.push_relative_error(ParseErrorCode::EofInTag, &source, cursor);
                    }
                } else {
                    let value_start = cursor;
                    while cursor < content_end && !bytes[cursor].is_ascii_whitespace() {
                        if matches!(bytes[cursor], b'"' | b'\'' | b'<' | b'=' | b'`') {
                            self.push_relative_error(
                                ParseErrorCode::UnexpectedCharacterInUnquotedAttributeValue,
                                &source,
                                cursor,
                            );
                        }
                        cursor += 1;
                    }
                    value = self.decode_references(
                        &source[value_start..cursor],
                        self.position.advanced_by(&source[..value_start]),
                        true,
                    );
                }
            }

            let duplicate = attributes.iter().any(|attribute: &SpannedAttribute| {
                attribute.name.eq_ignore_ascii_case(&attribute_name)
            });
            if duplicate {
                self.push_relative_error(
                    ParseErrorCode::DuplicateAttribute,
                    &source,
                    attribute_start,
                );
                continue;
            }
            if attributes.len() >= self.limits.max_attributes_per_tag {
                self.push_relative_error(
                    ParseErrorCode::AttributeLimitExceeded,
                    &source,
                    attribute_start,
                );
                continue;
            }
            attributes.push(SpannedAttribute {
                name: attribute_name,
                value,
                span: SourceSpan {
                    start: self.position.advanced_by(&source[..attribute_start]),
                    end: self.position.advanced_by(&source[..cursor]),
                },
            });
        }

        let span = SourceSpan::new(self.position, &source);
        self.consume_prefix(consumed);
        if !self_closing {
            self.enter_text_mode_for(&name);
        }
        output.push(SpannedToken {
            token: Token::StartTag {
                name,
                attributes,
                self_closing,
            },
            span,
        });
        true
    }

    fn enter_text_mode_for(&mut self, name: &str) {
        self.text_mode = match name {
            "script" | "style" | "xmp" | "iframe" | "noembed" | "noframes" => {
                Some(TextMode::RawText {
                    end_tag: name.to_owned(),
                })
            }
            "title" | "textarea" => Some(TextMode::RcData {
                end_tag: name.to_owned(),
            }),
            "plaintext" => Some(TextMode::PlainText),
            _ => None,
        };
    }

    /// Returns a complete tag length, zero after recovering an over-limit/EOF
    /// tag, or `None` when more input is required.
    fn complete_tag_length(&mut self, eof: bool) -> Option<usize> {
        if let Some(end) = find_tag_end(&self.pending) {
            return Some(end + 1);
        }
        if !eof && self.pending.len() <= self.limits.max_token_bytes {
            return None;
        }
        let code = if eof {
            ParseErrorCode::EofInTag
        } else {
            ParseErrorCode::TokenLimitExceeded
        };
        self.push_error(code, self.position);
        if eof {
            let length = self.pending.len();
            self.consume_prefix(length);
        } else {
            self.consume_prefix(1);
        }
        Some(0)
    }

    fn emit_characters(
        &mut self,
        length: usize,
        decode_entities: bool,
        output: &mut Vec<SpannedToken>,
    ) {
        let source = self.pending[..length].to_owned();
        let data = if decode_entities {
            self.decode_references(&source, self.position, false)
        } else {
            self.replace_nulls_and_normalize(&source, self.position)
        };
        let span = SourceSpan::new(self.position, &source);
        self.consume_prefix(length);
        if !data.is_empty() {
            output.push(SpannedToken {
                token: Token::Character(data),
                span,
            });
        }
    }

    fn emit_character_literal(&mut self, length: usize, output: &mut Vec<SpannedToken>) {
        let source = self.pending[..length].to_owned();
        let span = SourceSpan::new(self.position, &source);
        self.consume_prefix(length);
        output.push(SpannedToken {
            token: Token::Character(source),
            span,
        });
    }

    fn replace_nulls_and_normalize(&mut self, source: &str, start: SourcePosition) -> String {
        let mut output = String::with_capacity(source.len());
        let mut cursor = 0;
        while cursor < source.len() {
            let character = source[cursor..].chars().next().expect("valid UTF-8 suffix");
            if character == '\0' {
                self.push_error(
                    ParseErrorCode::UnexpectedNullCharacter,
                    start.advanced_by(&source[..cursor]),
                );
                output.push('\u{fffd}');
            } else if character == '\r' {
                output.push('\n');
                cursor += 1;
                if source[cursor..].starts_with('\n') {
                    cursor += 1;
                }
                continue;
            } else {
                output.push(character);
            }
            cursor += character.len_utf8();
        }
        output
    }

    fn decode_references(
        &mut self,
        source: &str,
        start: SourcePosition,
        in_attribute: bool,
    ) -> String {
        let mut output = String::with_capacity(source.len());
        let mut cursor = 0;
        while cursor < source.len() {
            if source.as_bytes()[cursor] != b'&' {
                let character = source[cursor..].chars().next().expect("valid UTF-8 suffix");
                if character == '\0' {
                    self.push_error(
                        ParseErrorCode::UnexpectedNullCharacter,
                        start.advanced_by(&source[..cursor]),
                    );
                    output.push('\u{fffd}');
                    cursor += 1;
                } else if character == '\r' {
                    output.push('\n');
                    cursor += 1;
                    if source[cursor..].starts_with('\n') {
                        cursor += 1;
                    }
                } else {
                    output.push(character);
                    cursor += character.len_utf8();
                }
                continue;
            }
            let reference_position = start.advanced_by(&source[..cursor]);
            let Some((replacement, consumed)) =
                self.consume_reference(&source[cursor..], reference_position, in_attribute)
            else {
                output.push('&');
                cursor += 1;
                continue;
            };
            output.push_str(&replacement);
            cursor += consumed;
        }
        output
    }

    fn consume_reference(
        &mut self,
        source: &str,
        position: SourcePosition,
        in_attribute: bool,
    ) -> Option<(String, usize)> {
        let bytes = source.as_bytes();
        if bytes.get(1) == Some(&b'#') {
            let mut cursor = 2;
            let hexadecimal = matches!(bytes.get(cursor), Some(b'x' | b'X'));
            if hexadecimal {
                cursor += 1;
            }
            let digits_start = cursor;
            while cursor < bytes.len()
                && if hexadecimal {
                    bytes[cursor].is_ascii_hexdigit()
                } else {
                    bytes[cursor].is_ascii_digit()
                }
            {
                cursor += 1;
            }
            if cursor == digits_start {
                self.push_error(
                    ParseErrorCode::AbsenceOfDigitsInNumericCharacterReference,
                    position,
                );
                return None;
            }
            let has_semicolon = bytes.get(cursor) == Some(&b';');
            if has_semicolon {
                cursor += 1;
            } else {
                self.push_error(
                    ParseErrorCode::MissingSemicolonAfterCharacterReference,
                    position,
                );
            }
            let radix = if hexadecimal { 16 } else { 10 };
            let value = u32::from_str_radix(
                &source[digits_start..cursor - usize::from(has_semicolon)],
                radix,
            )
            .unwrap_or(u32::MAX);
            let character =
                sanitize_numeric_reference(value, |code| self.push_error(code, position));
            return Some((character.to_string(), cursor));
        }

        let mut cursor = 1;
        while cursor < bytes.len() && bytes[cursor].is_ascii_alphanumeric() {
            cursor += 1;
        }
        if cursor == 1 {
            return None;
        }
        let has_semicolon = bytes.get(cursor) == Some(&b';');
        let name = &source[1..cursor];
        let Some(replacement) = named_reference(name) else {
            if has_semicolon {
                self.push_error(ParseErrorCode::UnknownNamedCharacterReference, position);
            }
            return None;
        };
        if !has_semicolon
            && in_attribute
            && bytes
                .get(cursor)
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'=')
        {
            return None;
        }
        if !has_semicolon {
            self.push_error(
                ParseErrorCode::MissingSemicolonAfterCharacterReference,
                position,
            );
        } else {
            cursor += 1;
        }
        Some((replacement.to_owned(), cursor))
    }

    fn consume_prefix(&mut self, length: usize) {
        let consumed = self.pending[..length].to_owned();
        self.position = self.position.advanced_by(&consumed);
        self.pending.drain(..length);
    }

    fn push_error(&mut self, code: ParseErrorCode, position: SourcePosition) {
        self.errors.push(ParseError {
            phase: ParsePhase::Tokenizer,
            code,
            position,
        });
    }

    fn push_relative_error(&mut self, code: ParseErrorCode, source: &str, offset: usize) {
        self.push_error(
            code,
            self.position
                .advanced_by(&source[..offset.min(source.len())]),
        );
    }
}

fn find_tag_end(source: &str) -> Option<usize> {
    let mut quote = None;
    for (index, byte) in source.bytes().enumerate().skip(1) {
        match (quote, byte) {
            (Some(expected), actual) if expected == actual => quote = None,
            (None, b'\'' | b'"') => quote = Some(byte),
            (None, b'>') => return Some(index),
            _ => {}
        }
    }
    None
}

fn safe_stream_text_prefix(source: &str, entities: bool, preserve_cr: bool) -> usize {
    let mut length = source.len();
    if preserve_cr && source.ends_with('\r') {
        length -= 1;
    }
    if entities && let Some(ampersand) = source[..length].rfind('&') {
        let tail = &source[ampersand + 1..length];
        if tail.is_empty()
            || tail
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'#' | b'x' | b'X'))
        {
            length = ampersand;
        }
    }
    length
}

fn floor_char_boundary(source: &str, mut index: usize) -> usize {
    while index > 0 && !source.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn find_appropriate_end_tag(source: &str, end_tag: &str) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut cursor = 0;
    while cursor + 2 + end_tag.len() <= bytes.len() {
        let relative = source[cursor..].find("</")?;
        let index = cursor + relative;
        let name_start = index + 2;
        let name_end = name_start + end_tag.len();
        if name_end > bytes.len() {
            return None;
        }
        if source[name_start..name_end].eq_ignore_ascii_case(end_tag)
            && bytes
                .get(name_end)
                .is_some_and(|byte| byte.is_ascii_whitespace() || matches!(*byte, b'/' | b'>'))
        {
            return Some(index);
        }
        cursor = index + 2;
    }
    None
}

fn starts_ascii_case_insensitive(source: &str, prefix: &str) -> bool {
    source
        .get(..prefix.len())
        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(prefix))
}

fn is_prefix_ascii_case_insensitive(source: &str, target: &str) -> bool {
    source.len() <= target.len()
        && source
            .bytes()
            .zip(target.bytes())
            .all(|(left, right)| left.eq_ignore_ascii_case(&right))
}

fn skip_ascii_whitespace(bytes: &[u8], cursor: &mut usize, end: usize) {
    while *cursor < end && bytes[*cursor].is_ascii_whitespace() {
        *cursor += 1;
    }
}

fn parse_quoted_identifier(
    source: &str,
    bytes: &[u8],
    cursor: &mut usize,
    end: usize,
) -> Option<String> {
    skip_ascii_whitespace(bytes, cursor, end);
    let quote = *bytes.get(*cursor)?;
    if !matches!(quote, b'\'' | b'"') {
        return None;
    }
    *cursor += 1;
    let start = *cursor;
    while *cursor < end && bytes[*cursor] != quote {
        *cursor += 1;
    }
    if *cursor >= end {
        return None;
    }
    let value = normalize_newlines(&source[start..*cursor]);
    *cursor += 1;
    Some(value)
}

fn normalize_newlines(source: &str) -> String {
    let mut output = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            output.push('\n');
        } else {
            output.push(character);
        }
    }
    output
}

fn named_reference(name: &str) -> Option<&'static str> {
    match name {
        "amp" | "AMP" => Some("&"),
        "lt" | "LT" => Some("<"),
        "gt" | "GT" => Some(">"),
        "quot" | "QUOT" => Some("\""),
        "apos" => Some("'"),
        "nbsp" => Some("\u{00a0}"),
        "copy" | "COPY" => Some("\u{00a9}"),
        "reg" | "REG" => Some("\u{00ae}"),
        "hellip" => Some("\u{2026}"),
        "ndash" => Some("\u{2013}"),
        "mdash" => Some("\u{2014}"),
        _ => None,
    }
}

fn sanitize_numeric_reference(value: u32, mut report: impl FnMut(ParseErrorCode)) -> char {
    if value == 0 {
        report(ParseErrorCode::NullCharacterReference);
        return '\u{fffd}';
    }
    if value > 0x10ffff {
        report(ParseErrorCode::CharacterReferenceOutsideUnicodeRange);
        return '\u{fffd}';
    }
    if (0xd800..=0xdfff).contains(&value) {
        report(ParseErrorCode::SurrogateCharacterReference);
        return '\u{fffd}';
    }
    if let Some(replacement) = windows_1252_replacement(value) {
        report(ParseErrorCode::ControlCharacterReference);
        return replacement;
    }
    char::from_u32(value).unwrap_or('\u{fffd}')
}

fn windows_1252_replacement(value: u32) -> Option<char> {
    Some(match value {
        0x80 => '\u{20ac}',
        0x82 => '\u{201a}',
        0x83 => '\u{0192}',
        0x84 => '\u{201e}',
        0x85 => '\u{2026}',
        0x86 => '\u{2020}',
        0x87 => '\u{2021}',
        0x88 => '\u{02c6}',
        0x89 => '\u{2030}',
        0x8a => '\u{0160}',
        0x8b => '\u{2039}',
        0x8c => '\u{0152}',
        0x8e => '\u{017d}',
        0x91 => '\u{2018}',
        0x92 => '\u{2019}',
        0x93 => '\u{201c}',
        0x94 => '\u{201d}',
        0x95 => '\u{2022}',
        0x96 => '\u{2013}',
        0x97 => '\u{2014}',
        0x98 => '\u{02dc}',
        0x99 => '\u{2122}',
        0x9a => '\u{0161}',
        0x9b => '\u{203a}',
        0x9c => '\u{0153}',
        0x9e => '\u{017e}',
        0x9f => '\u{0178}',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokenize(source: &str) -> (Vec<SpannedToken>, Vec<ParseError>) {
        let mut tokenizer = Tokenizer::default();
        let mut tokens = tokenizer.feed(source).unwrap();
        tokens.extend(tokenizer.finish().unwrap());
        (tokens, tokenizer.take_errors())
    }

    fn coalesce_characters(tokens: Vec<SpannedToken>) -> Vec<SpannedToken> {
        let mut result: Vec<SpannedToken> = Vec::new();
        for token in tokens {
            if let Some(SpannedToken {
                token: Token::Character(previous),
                span,
            }) = result.last_mut()
                && let Token::Character(data) = token.token
            {
                previous.push_str(&data);
                span.end = token.span.end;
            } else {
                result.push(token);
            }
        }
        result
    }

    #[test]
    fn incremental_boundaries_match_single_feed() {
        let source = "<!doctype html><!--c--><p a='x&amp;y'>one &lt; two</p>";
        let (expected_tokens, expected_errors) = tokenize(source);
        let expected_tokens = coalesce_characters(expected_tokens);
        for boundary in 0..=source.len() {
            let mut tokenizer = Tokenizer::default();
            let mut tokens = tokenizer.feed(&source[..boundary]).unwrap();
            tokens.extend(tokenizer.feed(&source[boundary..]).unwrap());
            tokens.extend(tokenizer.finish().unwrap());
            assert_eq!(
                (coalesce_characters(tokens), tokenizer.take_errors()),
                (expected_tokens.clone(), expected_errors.clone()),
                "boundary {boundary}"
            );
        }
    }

    #[test]
    fn raw_text_only_recognizes_appropriate_end_tag() {
        let (tokens, errors) = tokenize("<style>a<b>&amp;</styler>x</style><p>y");
        assert!(errors.is_empty());
        assert_eq!(
            tokens.iter().map(|token| &token.token).collect::<Vec<_>>(),
            vec![
                &Token::StartTag {
                    name: "style".into(),
                    attributes: vec![],
                    self_closing: false,
                },
                &Token::Character("a<b>&amp;</styler>x".into()),
                &Token::EndTag {
                    name: "style".into()
                },
                &Token::StartTag {
                    name: "p".into(),
                    attributes: vec![],
                    self_closing: false,
                },
                &Token::Character("y".into()),
            ]
        );
    }

    #[test]
    fn duplicate_attributes_are_dropped_and_references_decoded() {
        let (tokens, errors) = tokenize("<DIV ID=one id=two title='a&#x20AC;&copy;'>");
        let Token::StartTag {
            name, attributes, ..
        } = &tokens[0].token
        else {
            panic!("start tag expected");
        };
        assert_eq!(name, "div");
        assert_eq!(attributes.len(), 2);
        assert_eq!(attributes[0].name, "id");
        assert_eq!(attributes[0].value, "one");
        assert_eq!(attributes[1].value, "a€©");
        assert!(
            errors
                .iter()
                .any(|error| error.code == ParseErrorCode::DuplicateAttribute)
        );
    }

    #[test]
    fn slash_is_data_in_an_unquoted_attribute_value() {
        let (tokens, errors) = tokenize("<a href=/docs/page><img src=x />");
        assert!(errors.is_empty());
        let Token::StartTag {
            attributes,
            self_closing,
            ..
        } = &tokens[0].token
        else {
            panic!("start tag expected");
        };
        assert_eq!(attributes[0].value, "/docs/page");
        assert!(!self_closing);
        let Token::StartTag {
            attributes,
            self_closing,
            ..
        } = &tokens[1].token
        else {
            panic!("start tag expected");
        };
        assert_eq!(attributes[0].value, "x");
        assert!(*self_closing);
    }

    #[test]
    fn numeric_reference_errors_replace_unsafe_scalars() {
        let (tokens, errors) = tokenize("&#0; &#xD800; &#x110000; &#128;");
        assert_eq!(tokens[0].token, Token::Character("� � � €".into()));
        assert_eq!(
            errors.iter().map(|error| error.code).collect::<Vec<_>>(),
            vec![
                ParseErrorCode::NullCharacterReference,
                ParseErrorCode::SurrogateCharacterReference,
                ParseErrorCode::CharacterReferenceOutsideUnicodeRange,
                ParseErrorCode::ControlCharacterReference,
            ]
        );
    }

    #[test]
    fn source_positions_cross_crlf_and_unicode() {
        let (tokens, _) = tokenize("é\r\n<p>x");
        assert_eq!(tokens[1].span.start.byte, 4);
        assert_eq!(tokens[1].span.start.line, 2);
        assert_eq!(tokens[1].span.start.column, 1);
    }

    #[test]
    fn incomplete_constructs_report_eof_positions() {
        let (tokens, errors) = tokenize("one<!-- never closed");
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[1].token, Token::Comment(" never closed".into()));
        assert!(
            errors
                .iter()
                .any(|error| error.code == ParseErrorCode::EofInComment)
        );
    }

    #[test]
    fn configured_limits_report_and_recover() {
        let mut tokenizer = Tokenizer::new(TokenizerLimits {
            max_token_bytes: 8,
            max_attributes_per_tag: 1,
            ..TokenizerLimits::default()
        });
        let mut tokens = tokenizer.feed("<!--0123456789").unwrap();
        tokens.extend(tokenizer.feed("<p a=1 b=2>").unwrap());
        tokens.extend(tokenizer.finish().unwrap());
        let errors = tokenizer.take_errors();
        assert!(
            errors
                .iter()
                .any(|error| error.code == ParseErrorCode::TokenLimitExceeded)
        );
        assert!(
            errors
                .iter()
                .any(|error| error.code == ParseErrorCode::AttributeLimitExceeded)
        );
        let start_tag = tokens
            .iter()
            .find_map(|token| match &token.token {
                Token::StartTag { attributes, .. } => Some(attributes),
                _ => None,
            })
            .unwrap();
        assert_eq!(start_tag.len(), 1);
    }

    #[test]
    fn error_byte_positions_refer_to_original_crlf_source() {
        let (_, errors) = tokenize("x\r\n&bogus;");
        let error = errors
            .iter()
            .find(|error| error.code == ParseErrorCode::UnknownNamedCharacterReference)
            .unwrap();
        assert_eq!(error.position.byte, 3);
        assert_eq!(error.position.line, 2);
        assert_eq!(error.position.column, 1);
    }
}
