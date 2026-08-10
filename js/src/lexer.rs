use crate::error::SyntaxIssue;
use crate::source::{SourceLocation, SourceSpan};
use crate::string::{JsString, MAX_STRING_LENGTH};

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TokenKind {
    Identifier(String),
    Number(f64),
    String(JsString),
    Let,
    Const,
    Var,
    Function,
    New,
    Delete,
    Typeof,
    Return,
    If,
    Else,
    While,
    Do,
    For,
    Break,
    Continue,
    True,
    False,
    Null,
    This,
    Throw,
    Try,
    Catch,
    Finally,
    LeftBrace,
    RightBrace,
    LeftParen,
    RightParen,
    LeftBracket,
    RightBracket,
    Semicolon,
    Comma,
    Colon,
    Dot,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Bang,
    Assign,
    LessThan,
    GreaterThan,
    LessThanOrEqual,
    GreaterThanOrEqual,
    Equal,
    NotEqual,
    StrictEqual,
    StrictNotEqual,
    LogicalAnd,
    LogicalOr,
    Eof,
}

#[derive(Clone, Debug)]
pub(crate) struct Token {
    pub kind: TokenKind,
    pub span: SourceSpan,
}

pub(crate) struct Lexer<'source> {
    source: &'source str,
    offset: usize,
    line: u32,
    column: u32,
    previous_was_cr: bool,
}

impl<'source> Lexer<'source> {
    pub(crate) const fn new(source: &'source str) -> Self {
        Self {
            source,
            offset: 0,
            line: 1,
            column: 1,
            previous_was_cr: false,
        }
    }

    pub(crate) fn tokenize(mut self) -> Result<Vec<Token>, SyntaxIssue> {
        let mut tokens = Vec::new();
        loop {
            self.skip_trivia()?;
            let token = self.next_token()?;
            let is_eof = token.kind == TokenKind::Eof;
            tokens.push(token);
            if is_eof {
                return Ok(tokens);
            }
        }
    }

    fn next_token(&mut self) -> Result<Token, SyntaxIssue> {
        let start = self.location();
        let Some(character) = self.peek() else {
            return Ok(self.token(start, TokenKind::Eof));
        };

        if is_identifier_start(character) {
            return Ok(self.identifier(start));
        }
        if character.is_ascii_digit()
            || (character == '.' && self.peek_second().is_some_and(|next| next.is_ascii_digit()))
        {
            return self.number(start);
        }
        if character == '\'' || character == '"' {
            return self.string(start);
        }

        self.bump();
        let kind = match character {
            '{' => TokenKind::LeftBrace,
            '}' => TokenKind::RightBrace,
            '(' => TokenKind::LeftParen,
            ')' => TokenKind::RightParen,
            '[' => TokenKind::LeftBracket,
            ']' => TokenKind::RightBracket,
            ';' => TokenKind::Semicolon,
            ',' => TokenKind::Comma,
            ':' => TokenKind::Colon,
            '.' => TokenKind::Dot,
            '+' => TokenKind::Plus,
            '-' => TokenKind::Minus,
            '*' => TokenKind::Star,
            '/' => TokenKind::Slash,
            '%' => TokenKind::Percent,
            '!' => {
                if self.consume('=') {
                    if self.consume('=') {
                        TokenKind::StrictNotEqual
                    } else {
                        TokenKind::NotEqual
                    }
                } else {
                    TokenKind::Bang
                }
            }
            '=' => {
                if self.consume('=') {
                    if self.consume('=') {
                        TokenKind::StrictEqual
                    } else {
                        TokenKind::Equal
                    }
                } else {
                    TokenKind::Assign
                }
            }
            '<' => {
                if self.consume('=') {
                    TokenKind::LessThanOrEqual
                } else {
                    TokenKind::LessThan
                }
            }
            '>' => {
                if self.consume('=') {
                    TokenKind::GreaterThanOrEqual
                } else {
                    TokenKind::GreaterThan
                }
            }
            '&' if self.consume('&') => TokenKind::LogicalAnd,
            '|' if self.consume('|') => TokenKind::LogicalOr,
            '&' | '|' => {
                return Err(SyntaxIssue::new(
                    "bitwise operators are not implemented",
                    SourceSpan::new(start, self.location()),
                ));
            }
            _ => {
                return Err(SyntaxIssue::new(
                    format!("unexpected character {character:?}"),
                    SourceSpan::new(start, self.location()),
                ));
            }
        };
        Ok(self.token(start, kind))
    }

    fn skip_trivia(&mut self) -> Result<(), SyntaxIssue> {
        loop {
            while self.peek().is_some_and(char::is_whitespace) {
                self.bump();
            }
            if self.peek() != Some('/') {
                return Ok(());
            }
            match self.peek_second() {
                Some('/') => {
                    self.bump();
                    self.bump();
                    while self
                        .peek()
                        .is_some_and(|character| !is_line_terminator(character))
                    {
                        self.bump();
                    }
                }
                Some('*') => {
                    let start = self.location();
                    self.bump();
                    self.bump();
                    loop {
                        match (self.peek(), self.peek_second()) {
                            (Some('*'), Some('/')) => {
                                self.bump();
                                self.bump();
                                break;
                            }
                            (Some(_), _) => {
                                self.bump();
                            }
                            (None, _) => {
                                return Err(SyntaxIssue::new(
                                    "unterminated block comment",
                                    SourceSpan::new(start, self.location()),
                                ));
                            }
                        }
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    fn identifier(&mut self, start: SourceLocation) -> Token {
        let start_offset = self.offset;
        self.bump();
        while self.peek().is_some_and(is_identifier_continue) {
            self.bump();
        }
        let identifier = &self.source[start_offset..self.offset];
        let kind = match identifier {
            "let" => TokenKind::Let,
            "const" => TokenKind::Const,
            "var" => TokenKind::Var,
            "function" => TokenKind::Function,
            "new" => TokenKind::New,
            "delete" => TokenKind::Delete,
            "typeof" => TokenKind::Typeof,
            "return" => TokenKind::Return,
            "if" => TokenKind::If,
            "else" => TokenKind::Else,
            "while" => TokenKind::While,
            "do" => TokenKind::Do,
            "for" => TokenKind::For,
            "break" => TokenKind::Break,
            "continue" => TokenKind::Continue,
            "true" => TokenKind::True,
            "false" => TokenKind::False,
            "null" => TokenKind::Null,
            "this" => TokenKind::This,
            "throw" => TokenKind::Throw,
            "try" => TokenKind::Try,
            "catch" => TokenKind::Catch,
            "finally" => TokenKind::Finally,
            _ => TokenKind::Identifier(identifier.to_owned()),
        };
        self.token(start, kind)
    }

    fn number(&mut self, start: SourceLocation) -> Result<Token, SyntaxIssue> {
        let start_offset = self.offset;
        if self.peek() == Some('0')
            && let Some(prefix @ ('x' | 'X' | 'b' | 'B' | 'o' | 'O')) = self.peek_second()
        {
            self.bump();
            self.bump();
            let radix = match prefix {
                'x' | 'X' => 16,
                'b' | 'B' => 2,
                'o' | 'O' => 8,
                _ => {
                    return Err(SyntaxIssue::new(
                        "invalid numeric literal prefix",
                        SourceSpan::new(start, self.location()),
                    ));
                }
            };
            let digit_start = self.offset;
            while self
                .peek()
                .is_some_and(|character| character.is_digit(radix))
            {
                self.bump();
            }
            if digit_start == self.offset || self.peek().is_some_and(is_identifier_continue) {
                return Err(SyntaxIssue::new(
                    "invalid numeric literal",
                    SourceSpan::new(start, self.location()),
                ));
            }
            let digits = &self.source[digit_start..self.offset];
            let value = parse_radix_number(digits, radix).ok_or_else(|| {
                SyntaxIssue::new(
                    "invalid numeric literal",
                    SourceSpan::new(start, self.location()),
                )
            })?;
            return Ok(self.token(start, TokenKind::Number(value)));
        }

        while self
            .peek()
            .is_some_and(|character| character.is_ascii_digit())
        {
            self.bump();
        }
        if self.peek() == Some('.') {
            self.bump();
            while self
                .peek()
                .is_some_and(|character| character.is_ascii_digit())
            {
                self.bump();
            }
        }
        if self
            .peek()
            .is_some_and(|character| character == 'e' || character == 'E')
        {
            self.bump();
            if self
                .peek()
                .is_some_and(|character| character == '+' || character == '-')
            {
                self.bump();
            }
            let exponent_start = self.offset;
            while self
                .peek()
                .is_some_and(|character| character.is_ascii_digit())
            {
                self.bump();
            }
            if exponent_start == self.offset {
                return Err(SyntaxIssue::new(
                    "exponent has no digits",
                    SourceSpan::new(start, self.location()),
                ));
            }
        }
        if self.peek().is_some_and(is_identifier_start) {
            return Err(SyntaxIssue::new(
                "identifier cannot immediately follow a number",
                SourceSpan::new(start, self.location()),
            ));
        }
        let literal = &self.source[start_offset..self.offset];
        let value = literal.parse::<f64>().map_err(|_| {
            SyntaxIssue::new(
                "invalid numeric literal",
                SourceSpan::new(start, self.location()),
            )
        })?;
        Ok(self.token(start, TokenKind::Number(value)))
    }

    fn string(&mut self, start: SourceLocation) -> Result<Token, SyntaxIssue> {
        let Some(quote) = self.bump() else {
            return Err(SyntaxIssue::new(
                "expected a string quote",
                SourceSpan::new(start, self.location()),
            ));
        };
        let mut value = Vec::new();
        loop {
            let Some(character) = self.peek() else {
                return Err(SyntaxIssue::new(
                    "unterminated string literal",
                    SourceSpan::new(start, self.location()),
                ));
            };
            if character == quote {
                self.bump();
                let value = JsString::from_code_units(&value).map_err(|error| {
                    SyntaxIssue::new(error.to_string(), SourceSpan::new(start, self.location()))
                })?;
                return Ok(self.token(start, TokenKind::String(value)));
            }
            if is_line_terminator(character) && !matches!(character, '\u{2028}' | '\u{2029}') {
                return Err(SyntaxIssue::new(
                    "line terminator in string literal",
                    SourceSpan::new(start, self.location()),
                ));
            }
            self.bump();
            if character != '\\' {
                self.append_character(&mut value, character, start)?;
                continue;
            }
            let escape_start = self.location();
            let Some(escaped) = self.bump() else {
                return Err(SyntaxIssue::new(
                    "unterminated string escape",
                    SourceSpan::new(escape_start, self.location()),
                ));
            };
            match escaped {
                '\'' => self.push_code_unit(&mut value, u16::from(b'\''), escape_start)?,
                '"' => self.push_code_unit(&mut value, u16::from(b'"'), escape_start)?,
                '\\' => self.push_code_unit(&mut value, u16::from(b'\\'), escape_start)?,
                'n' => self.push_code_unit(&mut value, u16::from(b'\n'), escape_start)?,
                'r' => self.push_code_unit(&mut value, u16::from(b'\r'), escape_start)?,
                't' => self.push_code_unit(&mut value, u16::from(b'\t'), escape_start)?,
                'b' => self.push_code_unit(&mut value, 0x0008, escape_start)?,
                'f' => self.push_code_unit(&mut value, 0x000c, escape_start)?,
                'v' => self.push_code_unit(&mut value, 0x000b, escape_start)?,
                '0' if self.peek().is_none_or(|next| !next.is_ascii_digit()) => {
                    self.push_code_unit(&mut value, 0, escape_start)?;
                }
                'x' => {
                    let unit = self.fixed_hex_escape(2, escape_start)?;
                    self.push_code_unit(&mut value, unit, escape_start)?;
                }
                'u' => {
                    let code_point = if self.consume('{') {
                        self.braced_unicode_escape(escape_start)?
                    } else {
                        u32::from(self.fixed_hex_escape(4, escape_start)?)
                    };
                    self.append_code_point(&mut value, code_point, escape_start)?;
                }
                '\n' | '\u{2028}' | '\u{2029}' => {}
                '\r' => {
                    if self.peek() == Some('\n') {
                        self.bump();
                    }
                }
                '0'..='9' => {
                    return Err(SyntaxIssue::new(
                        "legacy decimal and octal string escapes are not implemented",
                        SourceSpan::new(escape_start, self.location()),
                    ));
                }
                other => self.append_character(&mut value, other, escape_start)?,
            }
        }
    }

    fn fixed_hex_escape(
        &mut self,
        digits: usize,
        start: SourceLocation,
    ) -> Result<u16, SyntaxIssue> {
        let mut value = 0_u16;
        for _ in 0..digits {
            let Some(character) = self.bump() else {
                return Err(SyntaxIssue::new(
                    "unterminated hexadecimal escape",
                    SourceSpan::new(start, self.location()),
                ));
            };
            let Some(digit) = character.to_digit(16) else {
                return Err(SyntaxIssue::new(
                    "invalid hexadecimal escape",
                    SourceSpan::new(start, self.location()),
                ));
            };
            value = value * 16 + u16::try_from(digit).expect("hexadecimal digit fits in u16");
        }
        Ok(value)
    }

    fn braced_unicode_escape(&mut self, start: SourceLocation) -> Result<u32, SyntaxIssue> {
        let mut value = 0_u32;
        let mut saw_digit = false;
        let mut overflowed = false;
        while let Some(character) = self.peek() {
            let Some(digit) = character.to_digit(16) else {
                break;
            };
            self.bump();
            saw_digit = true;
            if !overflowed {
                overflowed = value
                    .checked_mul(16)
                    .and_then(|value| value.checked_add(digit))
                    .is_none_or(|next| {
                        if next > 0x10_ffff {
                            true
                        } else {
                            value = next;
                            false
                        }
                    });
            }
        }
        if !saw_digit || overflowed || !self.consume('}') {
            return Err(SyntaxIssue::new(
                "invalid braced Unicode escape",
                SourceSpan::new(start, self.location()),
            ));
        }
        Ok(value)
    }

    fn append_character(
        &self,
        value: &mut Vec<u16>,
        character: char,
        start: SourceLocation,
    ) -> Result<(), SyntaxIssue> {
        let mut encoded = [0; 2];
        let units = character.encode_utf16(&mut encoded);
        self.append_units(value, units, start)
    }

    fn append_code_point(
        &self,
        value: &mut Vec<u16>,
        code_point: u32,
        start: SourceLocation,
    ) -> Result<(), SyntaxIssue> {
        if let Ok(unit) = u16::try_from(code_point) {
            return self.push_code_unit(value, unit, start);
        }
        let supplementary = code_point - 0x1_0000;
        let lead =
            0xd800 + u16::try_from(supplementary >> 10).expect("lead surrogate offset fits in u16");
        let trail = 0xdc00
            + u16::try_from(supplementary & 0x3ff).expect("trail surrogate offset fits in u16");
        self.append_units(value, &[lead, trail], start)
    }

    fn append_units(
        &self,
        value: &mut Vec<u16>,
        units: &[u16],
        start: SourceLocation,
    ) -> Result<(), SyntaxIssue> {
        let length = value.len().checked_add(units.len()).ok_or_else(|| {
            SyntaxIssue::new(
                "JavaScript string length overflow",
                SourceSpan::new(start, self.location()),
            )
        })?;
        if length > MAX_STRING_LENGTH as usize {
            return Err(SyntaxIssue::new(
                "JavaScript string literal exceeds the supported length",
                SourceSpan::new(start, self.location()),
            ));
        }
        value.extend_from_slice(units);
        Ok(())
    }

    fn push_code_unit(
        &self,
        value: &mut Vec<u16>,
        unit: u16,
        start: SourceLocation,
    ) -> Result<(), SyntaxIssue> {
        self.append_units(value, &[unit], start)
    }

    fn token(&self, start: SourceLocation, kind: TokenKind) -> Token {
        Token {
            kind,
            span: SourceSpan::new(start, self.location()),
        }
    }

    const fn location(&self) -> SourceLocation {
        SourceLocation {
            line: self.line,
            column: self.column,
            byte_offset: self.offset,
        }
    }

    fn peek(&self) -> Option<char> {
        self.source[self.offset..].chars().next()
    }

    fn peek_second(&self) -> Option<char> {
        let mut characters = self.source[self.offset..].chars();
        characters.next()?;
        characters.next()
    }

    fn consume(&mut self, expected: char) -> bool {
        if self.peek() == Some(expected) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn bump(&mut self) -> Option<char> {
        let character = self.peek()?;
        self.offset += character.len_utf8();
        match character {
            '\r' => {
                self.line = self.line.saturating_add(1);
                self.column = 1;
                self.previous_was_cr = true;
            }
            '\n' if self.previous_was_cr => {
                self.previous_was_cr = false;
            }
            '\n' | '\u{2028}' | '\u{2029}' => {
                self.line = self.line.saturating_add(1);
                self.column = 1;
                self.previous_was_cr = false;
            }
            _ => {
                self.column = self.column.saturating_add(1);
                self.previous_was_cr = false;
            }
        }
        Some(character)
    }
}

fn is_identifier_start(character: char) -> bool {
    character == '$' || character == '_' || character.is_alphabetic()
}

const fn is_line_terminator(character: char) -> bool {
    matches!(character, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

fn is_identifier_continue(character: char) -> bool {
    is_identifier_start(character) || character.is_alphanumeric()
}

fn parse_radix_number(digits: &str, radix: u32) -> Option<f64> {
    digits.chars().try_fold(0.0_f64, |value, character| {
        character
            .to_digit(radix)
            .map(|digit| value.mul_add(f64::from(radix), f64::from(digit)))
    })
}

#[cfg(test)]
mod tests {
    use super::{Lexer, TokenKind};
    use crate::string::JsString;

    #[test]
    fn tracks_unicode_columns_and_crlf_as_one_line() {
        let tokens = Lexer::new("let café = 1;\r\ncafé").tokenize().unwrap();
        assert_eq!(tokens[1].kind, TokenKind::Identifier("café".to_owned()));
        assert_eq!(tokens[5].span.start.line, 2);
        assert_eq!(tokens[5].span.start.column, 1);
    }

    #[test]
    fn tokenizes_longest_operators_and_comments() {
        let tokens = Lexer::new("1 !== 2 && /* x */ 3 === 3 // y\n || false")
            .tokenize()
            .unwrap();
        let kinds: Vec<_> = tokens.into_iter().map(|token| token.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::Number(1.0),
                TokenKind::StrictNotEqual,
                TokenKind::Number(2.0),
                TokenKind::LogicalAnd,
                TokenKind::Number(3.0),
                TokenKind::StrictEqual,
                TokenKind::Number(3.0),
                TokenKind::LogicalOr,
                TokenKind::False,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn unicode_separators_terminate_single_line_comments() {
        let tokens = Lexer::new("// ignored\u{2028}let x = 1; // ignored\u{2029}x;")
            .tokenize()
            .unwrap();
        assert_eq!(tokens[0].kind, TokenKind::Let);
        assert_eq!(tokens[0].span.start.line, 2);
        assert_eq!(tokens[5].kind, TokenKind::Identifier("x".to_owned()));
        assert_eq!(tokens[5].span.start.line, 3);
    }

    #[test]
    fn decodes_strings_and_radix_numbers() {
        let tokens = Lexer::new(r#""a\n\x62\u0063" 0xff 0b10 0o10 .5 1e2"#)
            .tokenize()
            .unwrap();
        assert_eq!(
            tokens[0].kind,
            TokenKind::String(JsString::from_utf8("a\nbc").unwrap())
        );
        assert_eq!(tokens[1].kind, TokenKind::Number(255.0));
        assert_eq!(tokens[2].kind, TokenKind::Number(2.0));
        assert_eq!(tokens[3].kind, TokenKind::Number(8.0));
        assert_eq!(tokens[4].kind, TokenKind::Number(0.5));
        assert_eq!(tokens[5].kind, TokenKind::Number(100.0));
    }

    #[test]
    fn decodes_string_literals_as_exact_utf16_code_units() {
        let tokens = Lexer::new(r#""\uD800\u{D800}\u{DFFF}\u{00010000}\u{10FFFF}\x00𐒠""#)
            .tokenize()
            .unwrap();
        let TokenKind::String(value) = &tokens[0].kind else {
            panic!("expected a string token");
        };
        assert_eq!(
            value.as_code_units(),
            &[
                0xd800, 0xd800, 0xdfff, 0xd800, 0xdc00, 0xdbff, 0xdfff, 0, 0xd801, 0xdca0,
            ]
        );
    }

    #[test]
    fn unicode_separators_are_literals_or_line_continuations_and_track_lines() {
        let tokens = Lexer::new("\"a\u{2028}b\\\u{2029}c\";\u{2028}\"d\"")
            .tokenize()
            .unwrap();
        assert_eq!(
            tokens[0].kind,
            TokenKind::String(JsString::from_utf8("a\u{2028}bc").unwrap())
        );
        assert_eq!(tokens[0].span.end.line, 3);
        assert_eq!(tokens[2].span.start.line, 4);
    }

    #[test]
    fn unsupported_legacy_decimal_and_octal_escapes_fail_closed() {
        for source in [r#""\1""#, r#""\01""#, r#""\08""#, r#""\9""#] {
            assert!(
                Lexer::new(source).tokenize().is_err(),
                "unsupported legacy escape was accepted: {source}"
            );
        }
    }

    #[test]
    fn braced_unicode_escapes_reject_malformed_or_out_of_range_values() {
        for source in [
            r#""\u{}""#,
            r#""\u{G}""#,
            r#""\u{110000}""#,
            r#""\u{00110000}""#,
            r#""\u{FF FF}""#,
            r#""\u{FF_FF}""#,
            r#""\u{FFFF""#,
            r#""\u12""#,
        ] {
            assert!(
                Lexer::new(source).tokenize().is_err(),
                "malformed escape was accepted: {source}"
            );
        }
    }
}
