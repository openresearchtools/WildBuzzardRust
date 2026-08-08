use crate::error::SyntaxIssue;
use crate::source::{SourceLocation, SourceSpan};

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TokenKind {
    Identifier(String),
    Number(f64),
    String(String),
    Let,
    Const,
    Function,
    New,
    Delete,
    Return,
    If,
    Else,
    While,
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
                        .is_some_and(|character| character != '\n' && character != '\r')
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
            "function" => TokenKind::Function,
            "new" => TokenKind::New,
            "delete" => TokenKind::Delete,
            "return" => TokenKind::Return,
            "if" => TokenKind::If,
            "else" => TokenKind::Else,
            "while" => TokenKind::While,
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
        let mut value = String::new();
        loop {
            let Some(character) = self.peek() else {
                return Err(SyntaxIssue::new(
                    "unterminated string literal",
                    SourceSpan::new(start, self.location()),
                ));
            };
            if character == quote {
                self.bump();
                return Ok(self.token(start, TokenKind::String(value)));
            }
            if character == '\n' || character == '\r' {
                return Err(SyntaxIssue::new(
                    "line terminator in string literal",
                    SourceSpan::new(start, self.location()),
                ));
            }
            self.bump();
            if character != '\\' {
                value.push(character);
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
                '\'' => value.push('\''),
                '"' => value.push('"'),
                '\\' => value.push('\\'),
                'n' => value.push('\n'),
                'r' => value.push('\r'),
                't' => value.push('\t'),
                'b' => value.push('\u{0008}'),
                'f' => value.push('\u{000c}'),
                'v' => value.push('\u{000b}'),
                '0' if self.peek().is_none_or(|next| !next.is_ascii_digit()) => value.push('\0'),
                'x' => value.push(self.hex_escape(2, escape_start)?),
                'u' => value.push(self.hex_escape(4, escape_start)?),
                '\n' => {}
                '\r' => {
                    if self.peek() == Some('\n') {
                        self.bump();
                    }
                }
                other => value.push(other),
            }
        }
    }

    fn hex_escape(&mut self, digits: usize, start: SourceLocation) -> Result<char, SyntaxIssue> {
        let mut value = 0_u32;
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
            value = value * 16 + digit;
        }
        char::from_u32(value).ok_or_else(|| {
            SyntaxIssue::new(
                "lone UTF-16 surrogate escapes are not implemented",
                SourceSpan::new(start, self.location()),
            )
        })
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
            '\n' => {
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
    fn decodes_strings_and_radix_numbers() {
        let tokens = Lexer::new(r#""a\n\x62\u0063" 0xff 0b10 0o10 .5 1e2"#)
            .tokenize()
            .unwrap();
        assert_eq!(tokens[0].kind, TokenKind::String("a\nbc".to_owned()));
        assert_eq!(tokens[1].kind, TokenKind::Number(255.0));
        assert_eq!(tokens[2].kind, TokenKind::Number(2.0));
        assert_eq!(tokens[3].kind, TokenKind::Number(8.0));
        assert_eq!(tokens[4].kind, TokenKind::Number(0.5));
        assert_eq!(tokens[5].kind, TokenKind::Number(100.0));
    }
}
