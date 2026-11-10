//! The GraphQL lexer.
//!
//! The lexer turns a source document into a stream of tokens. Tokens borrow names, numbers and
//! unescaped string contents from the source; strings that need unescaping and block strings are
//! owned.
//!
//! Every token carries the 1-indexed line and column of its first character. Columns count Unicode
//! scalar values rather than bytes, matching the specification.

use std::borrow::Cow;

use crate::{GraphQLError, Location};

/// A punctuator token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Punct {
    /// `!`
    Bang,
    /// `$`
    Dollar,
    /// `&`
    Amp,
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `...`
    Spread,
    /// `:`
    Colon,
    /// `=`
    Equals,
    /// `@`
    At,
    /// `[`
    LBracket,
    /// `]`
    RBracket,
    /// `{`
    LBrace,
    /// `|`
    Pipe,
    /// `}`
    RBrace,
}

impl Punct {
    /// The source text of the punctuator.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Bang => "!",
            Self::Dollar => "$",
            Self::Amp => "&",
            Self::LParen => "(",
            Self::RParen => ")",
            Self::Spread => "...",
            Self::Colon => ":",
            Self::Equals => "=",
            Self::At => "@",
            Self::LBracket => "[",
            Self::RBracket => "]",
            Self::LBrace => "{",
            Self::Pipe => "|",
            Self::RBrace => "}",
        }
    }
}

/// The kind of a token, with any borrowed payload.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TokenKind<'a> {
    /// A name, with the source text.
    Name(&'a str),
    /// An integer literal, with the raw source text.
    Int(&'a str),
    /// A float literal, with the raw source text.
    Float(&'a str),
    /// A single-line string literal, with its unescaped value.
    String(Cow<'a, str>),
    /// A block string literal, with its dedented value.
    BlockString(Cow<'a, str>),
    /// A punctuator.
    Punct(Punct),
    /// The end of the source.
    Eof,
}

/// A token and where it starts.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Token<'a> {
    /// The token kind.
    pub(crate) kind: TokenKind<'a>,
    /// Where the token starts.
    pub(crate) location: Location,
}

/// A lexer over a GraphQL source document.
pub(crate) struct Lexer<'a> {
    source: &'a str,
    pos: usize,
    line: u32,
    column: u32,
}

impl<'a> Lexer<'a> {
    /// Create a lexer positioned at the start of `source`.
    pub(crate) fn new(source: &'a str) -> Self {
        Self {
            source,
            pos: 0,
            line: 1,
            column: 1,
        }
    }

    /// The location of the next character to be read.
    fn location(&self) -> Location {
        Location::new(self.line, self.column)
    }

    /// The unread remainder of the source.
    fn rest(&self) -> &'a str {
        self.source.get(self.pos..).unwrap_or("")
    }

    /// The next character, without consuming it.
    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    /// Consume one character, maintaining the line and column counters.
    ///
    /// Line terminators are normalised: both `\r\n` and a lone `\r` are reported as `\n`, and are
    /// consumed as a single character.
    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.pos = self.pos.saturating_add(ch.len_utf8());
        match ch {
            '\r' => {
                if self.rest().starts_with('\n') {
                    self.pos = self.pos.saturating_add(1);
                }
                self.line = self.line.saturating_add(1);
                self.column = 1;
                Some('\n')
            }
            '\n' => {
                self.line = self.line.saturating_add(1);
                self.column = 1;
                Some('\n')
            }
            _ => {
                self.column = self.column.saturating_add(1);
                Some(ch)
            }
        }
    }

    /// Skip whitespace, line terminators, commas, comments and a leading byte order mark.
    ///
    /// Ignored characters are all ASCII except the byte order mark, so this walks bytes and only
    /// falls back to the character-wise path for the BOM. Documents are mostly whitespace, and
    /// this runs before every token, so runs of spaces are skipped in one step.
    fn skip_ignored(&mut self) {
        let bytes = self.source.as_bytes();
        loop {
            let Some(&byte) = bytes.get(self.pos) else {
                return;
            };
            match ignored_class(byte) {
                IGNORED_PLAIN => {
                    let start = self.pos;
                    let mut end = start.saturating_add(1);
                    while let Some(&next) = bytes.get(end) {
                        if ignored_class(next) != IGNORED_PLAIN {
                            break;
                        }
                        end = end.saturating_add(1);
                    }
                    self.pos = end;
                    self.column = self
                        .column
                        .saturating_add(u32::try_from(end - start).unwrap_or(u32::MAX));
                }
                IGNORED_NEWLINE => {
                    self.pos = self.pos.saturating_add(1);
                    self.line = self.line.saturating_add(1);
                    self.column = 1;
                }
                IGNORED_CARRIAGE_RETURN => {
                    self.pos = self.pos.saturating_add(1);
                    if bytes.get(self.pos) == Some(&b'\n') {
                        self.pos = self.pos.saturating_add(1);
                    }
                    self.line = self.line.saturating_add(1);
                    self.column = 1;
                }
                IGNORED_COMMENT => {
                    // Comments may contain any character, so they keep the character-wise walk
                    // that maintains line and column counters.
                    while let Some(ch) = self.peek() {
                        if ch == '\n' || ch == '\r' {
                            break;
                        }
                        self.bump();
                    }
                }
                _ => {
                    if byte < 0x80 {
                        return;
                    }
                    // A byte order mark is the only non-ASCII character ignored here.
                    if !self.consume_bom() {
                        return;
                    }
                }
            }
        }
    }

    /// Consume a leading byte order mark, if the source has one here.
    fn consume_bom(&mut self) -> bool {
        if self.rest().starts_with('\u{feff}') {
            self.pos = self.pos.saturating_add('\u{feff}'.len_utf8());
            self.column = self.column.saturating_add(1);
            true
        } else {
            false
        }
    }

    /// Produce the next token.
    pub(crate) fn next(&mut self) -> Result<Token<'a>, GraphQLError> {
        self.skip_ignored();
        let location = self.location();
        let Some(&byte) = self.source.as_bytes().get(self.pos) else {
            return Ok(Token {
                kind: TokenKind::Eof,
                location,
            });
        };

        // Punctuators and names are ASCII, and they are the overwhelming majority of tokens, so
        // the byte is classified directly instead of decoding a `char` first.
        let kind = if byte.is_ascii() {
            match byte {
                b'!' => self.punctuator_ascii(Punct::Bang),
                b'$' => self.punctuator_ascii(Punct::Dollar),
                b'&' => self.punctuator_ascii(Punct::Amp),
                b'(' => self.punctuator_ascii(Punct::LParen),
                b')' => self.punctuator_ascii(Punct::RParen),
                b':' => self.punctuator_ascii(Punct::Colon),
                b'=' => self.punctuator_ascii(Punct::Equals),
                b'@' => self.punctuator_ascii(Punct::At),
                b'[' => self.punctuator_ascii(Punct::LBracket),
                b']' => self.punctuator_ascii(Punct::RBracket),
                b'{' => self.punctuator_ascii(Punct::LBrace),
                b'|' => self.punctuator_ascii(Punct::Pipe),
                b'}' => self.punctuator_ascii(Punct::RBrace),
                b'.' => self.spread(location)?,
                b'"' => {
                    if self.rest().starts_with("\"\"\"") {
                        self.block_string()?
                    } else {
                        self.string()?
                    }
                }
                b'-' | b'0'..=b'9' => self.number(location)?,
                byte if is_name_start_byte(byte) => self.name(),
                b'\'' => {
                    return Err(self.error(
                        location,
                        "Unexpected single quote character ('), did you mean to use a double quote (\")?"
                            .to_owned(),
                    ));
                }
                byte => {
                    return Err(self.error(
                        location,
                        format!("Unexpected character: {}.", print_code_point(byte as char)),
                    ));
                }
            }
        } else {
            let Some(ch) = self.peek() else {
                return Ok(Token {
                    kind: TokenKind::Eof,
                    location,
                });
            };
            match ch {
                ch if is_name_start(ch) => self.name(),
                ch => {
                    return Err(self.error(
                        location,
                        format!("Unexpected character: {}.", print_code_point(ch)),
                    ));
                }
            }
        };

        Ok(Token { kind, location })
    }

    /// Consume a single-character ASCII punctuator.
    fn punctuator_ascii(&mut self, punct: Punct) -> TokenKind<'a> {
        self.pos = self.pos.saturating_add(1);
        self.column = self.column.saturating_add(1);
        TokenKind::Punct(punct)
    }

    /// Consume a `...` spread, rejecting stray dots.
    fn spread(&mut self, location: Location) -> Result<TokenKind<'a>, GraphQLError> {
        if self.rest().starts_with("...") {
            self.pos = self.pos.saturating_add(3);
            self.column = self.column.saturating_add(3);
            Ok(TokenKind::Punct(Punct::Spread))
        } else {
            Err(self.error(location, "Unexpected character: \".\".".to_owned()))
        }
    }

    /// Consume a name.
    ///
    /// Names are ASCII, so scanning bytes avoids decoding a `char` per character, and the whole
    /// run is counted once instead of updating the column per byte.
    fn name(&mut self) -> TokenKind<'a> {
        let bytes = self.source.as_bytes();
        let start = self.pos;
        let mut end = start;
        while let Some(&byte) = bytes.get(end) {
            if !is_name_continue_byte(byte) {
                break;
            }
            end = end.saturating_add(1);
        }
        self.pos = end;
        self.column = self
            .column
            .saturating_add(u32::try_from(end - start).unwrap_or(u32::MAX));
        TokenKind::Name(self.source.get(start..end).unwrap_or(""))
    }

    /// Consume an integer or float literal.
    fn number(&mut self, _location: Location) -> Result<TokenKind<'a>, GraphQLError> {
        let start = self.pos;
        let mut is_float = false;

        if self.peek() == Some('-') {
            self.bump();
        }

        match self.peek() {
            Some('0') => {
                self.bump();
                if let Some(ch) = self.peek() {
                    if ch.is_ascii_digit() {
                        return Err(self.error(
                            self.location(),
                            format!(
                                "Invalid number, unexpected digit after 0: {}.",
                                print_code_point(ch)
                            ),
                        ));
                    }
                }
            }
            Some(ch) if ch.is_ascii_digit() => {
                self.digits();
            }
            _ => {
                return Err(self.error(
                    self.location(),
                    format!(
                        "Invalid number, expected digit but got: {}.",
                        self.print_next()
                    ),
                ));
            }
        }

        if self.peek() == Some('.') {
            is_float = true;
            self.bump();
            if !matches!(self.peek(), Some(ch) if ch.is_ascii_digit()) {
                return Err(self.error(
                    self.location(),
                    format!(
                        "Invalid number, expected digit but got: {}.",
                        self.print_next()
                    ),
                ));
            }
            self.digits();
        }

        if matches!(self.peek(), Some('e' | 'E')) {
            is_float = true;
            self.bump();
            if matches!(self.peek(), Some('+' | '-')) {
                self.bump();
            }
            if !matches!(self.peek(), Some(ch) if ch.is_ascii_digit()) {
                return Err(self.error(
                    self.location(),
                    format!(
                        "Invalid number, expected digit but got: {}.",
                        self.print_next()
                    ),
                ));
            }
            self.digits();
        }

        if let Some(ch) = self.peek() {
            if is_name_start(ch) || ch == '.' {
                return Err(self.error(
                    self.location(),
                    format!(
                        "Invalid number, expected digit but got: {}.",
                        print_code_point(ch)
                    ),
                ));
            }
        }

        let text = self.source.get(start..self.pos).unwrap_or("");
        Ok(if is_float {
            TokenKind::Float(text)
        } else {
            TokenKind::Int(text)
        })
    }

    /// Consume a run of ASCII digits.
    fn digits(&mut self) {
        while matches!(self.source.as_bytes().get(self.pos), Some(b'0'..=b'9')) {
            self.pos = self.pos.saturating_add(1);
            self.column = self.column.saturating_add(1);
        }
    }

    /// Describe the next character for a lexer error.
    fn print_next(&self) -> String {
        self.peek()
            .map_or_else(|| "<EOF>".to_owned(), print_code_point)
    }

    /// Consume a single-line string literal.
    fn string(&mut self) -> Result<TokenKind<'a>, GraphQLError> {
        self.bump(); // opening quote
        let content_start = self.pos;
        let mut chunk_start = self.pos;
        let mut value: Option<String> = None;

        loop {
            let Some(ch) = self.peek() else {
                return Err(self.error(self.location(), "Unterminated string.".to_owned()));
            };
            match ch {
                '"' => {
                    let chunk = self.source.get(chunk_start..self.pos).unwrap_or("");
                    let value = match value {
                        Some(mut value) => {
                            value.push_str(chunk);
                            Cow::Owned(value)
                        }
                        None => Cow::Borrowed(chunk),
                    };
                    self.bump(); // closing quote
                    return Ok(TokenKind::String(value));
                }
                '\\' => {
                    let owned = value.get_or_insert_with(|| {
                        String::with_capacity(
                            self.pos.saturating_sub(content_start).saturating_add(8),
                        )
                    });
                    owned.push_str(self.source.get(chunk_start..self.pos).unwrap_or(""));
                    self.bump(); // backslash
                    owned.push(self.escape()?);
                    chunk_start = self.pos;
                }
                '\n' | '\r' => {
                    return Err(self.error(self.location(), "Unterminated string.".to_owned()));
                }
                ch => {
                    if (ch as u32) < 0x20 && ch != '\t' {
                        return Err(self.error(
                            self.location(),
                            format!("Invalid character within String: {}.", print_code_point(ch)),
                        ));
                    }
                    self.bump();
                }
            }
        }
    }

    /// Consume one escape sequence, returning the character it denotes.
    fn escape(&mut self) -> Result<char, GraphQLError> {
        let escape_location = Location::new(self.line, self.column.saturating_sub(1));
        let escape_start = self.pos.saturating_sub(1);
        let Some(ch) = self.peek() else {
            return Err(self.error(
                escape_location,
                "Invalid character escape sequence: \"\\\".".to_owned(),
            ));
        };
        let value = match ch {
            '"' => '"',
            '\\' => '\\',
            '/' => '/',
            'b' => '\u{0008}',
            'f' => '\u{000c}',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'u' => {
                self.bump();
                return self.unicode_escape(escape_start, escape_location);
            }
            other => {
                self.bump();
                return Err(self.error(
                    escape_location,
                    format!("Invalid character escape sequence: \"\\{other}\"."),
                ));
            }
        };
        self.bump();
        Ok(value)
    }

    /// Consume the digits of a `\u` escape, combining surrogate pairs.
    fn unicode_escape(
        &mut self,
        escape_start: usize,
        escape_location: Location,
    ) -> Result<char, GraphQLError> {
        if self.peek() == Some('{') {
            return self.unicode_escape_variable(escape_start, escape_location);
        }

        let Some(high) = self.hex4() else {
            return Err(self.error(
                escape_location,
                format!(
                    "Invalid Unicode escape sequence: \"{}\".",
                    self.escape_text(escape_start)
                ),
            ));
        };

        if (0xd800..=0xdbff).contains(&high) {
            if !self.rest().starts_with("\\u") {
                return Err(self.error(
                    escape_location,
                    format!(
                        "Invalid Unicode escape sequence: \"{}\".",
                        self.escape_text(escape_start)
                    ),
                ));
            }
            self.bump();
            self.bump();
            let Some(low) = self.hex4() else {
                return Err(self.error(
                    escape_location,
                    format!(
                        "Invalid Unicode escape sequence: \"{}\".",
                        self.escape_text(escape_start)
                    ),
                ));
            };
            if !(0xdc00..=0xdfff).contains(&low) {
                return Err(self.error(
                    escape_location,
                    format!(
                        "Invalid Unicode escape sequence: \"{}\".",
                        self.escape_text(escape_start)
                    ),
                ));
            }
            let scalar = 0x1_0000 + ((u32::from(high) - 0xd800) << 10) + (u32::from(low) - 0xdc00);
            char::from_u32(scalar).ok_or_else(|| {
                self.error(
                    escape_location,
                    format!(
                        "Invalid Unicode escape sequence: \"{}\".",
                        self.escape_text(escape_start)
                    ),
                )
            })
        } else if (0xdc00..=0xdfff).contains(&high) {
            Err(self.error(
                escape_location,
                format!(
                    "Invalid Unicode escape sequence: \"{}\".",
                    self.escape_text(escape_start)
                ),
            ))
        } else {
            char::from_u32(u32::from(high)).ok_or_else(|| {
                self.error(
                    escape_location,
                    format!(
                        "Invalid Unicode escape sequence: \"{}\".",
                        self.escape_text(escape_start)
                    ),
                )
            })
        }
    }

    /// Consume the hexadecimal digits of the variable-width `\u{...}` escape.
    fn unicode_escape_variable(
        &mut self,
        escape_start: usize,
        escape_location: Location,
    ) -> Result<char, GraphQLError> {
        self.bump(); // `{`
        let mut value = 0u64;
        let mut digits = 0u32;

        loop {
            match self.peek() {
                Some('}') => {
                    self.bump();
                    if digits == 0 {
                        return Err(self.invalid_unicode(escape_start, escape_location));
                    }
                    return char::from_u32(u32::try_from(value).unwrap_or(u32::MAX))
                        .ok_or_else(|| self.invalid_unicode(escape_start, escape_location));
                }
                Some(ch) if ch.is_ascii_hexdigit() => {
                    let digit = ch.to_digit(16).unwrap_or(0);
                    value = value.saturating_mul(16).saturating_add(u64::from(digit));
                    digits = digits.saturating_add(1);
                    self.bump();
                    // The reference implementation caps the whole escape at 12 characters, which
                    // allows at most eight hexadecimal digits.
                    if digits > 8 {
                        return Err(self.invalid_unicode(escape_start, escape_location));
                    }
                }
                Some(_) => {
                    self.bump();
                    return Err(self.invalid_unicode(escape_start, escape_location));
                }
                None => return Err(self.invalid_unicode(escape_start, escape_location)),
            }
        }
    }

    /// Build an error for the consumed portion of a Unicode escape.
    fn invalid_unicode(&self, escape_start: usize, escape_location: Location) -> GraphQLError {
        let sequence = self.source.get(escape_start..self.pos).unwrap_or("");
        self.error(
            escape_location,
            format!("Invalid Unicode escape sequence: \"{sequence}\"."),
        )
    }

    /// Read exactly four ASCII hexadecimal digits.
    fn hex4(&mut self) -> Option<u16> {
        let mut value = 0u16;
        for _ in 0..4 {
            let ch = self.peek()?;
            if !ch.is_ascii_hexdigit() {
                return None;
            }
            let digit = ch.to_digit(16)?;
            value = (value << 4) | u16::try_from(digit).ok()?;
            self.bump();
        }
        Some(value)
    }

    /// The raw text of an escape sequence, truncated for an error message.
    fn escape_text(&self, start: usize) -> String {
        self.source
            .get(start..)
            .unwrap_or("")
            .chars()
            .take(6)
            .collect()
    }

    /// Consume a block string literal and apply the specification's dedent algorithm.
    fn block_string(&mut self) -> Result<TokenKind<'a>, GraphQLError> {
        self.bump();
        self.bump();
        self.bump();

        let mut raw = String::new();
        loop {
            let Some(ch) = self.peek() else {
                return Err(self.error(self.location(), "Unterminated string.".to_owned()));
            };
            if ch == '"' && self.rest().starts_with("\"\"\"") {
                self.bump();
                self.bump();
                self.bump();
                break;
            }
            if ch == '\\' && self.rest().starts_with("\\\"\"\"") {
                self.bump();
                self.bump();
                self.bump();
                self.bump();
                raw.push_str("\"\"\"");
                continue;
            }
            // October 2021 2.9.4: a block string may contain tabs, line terminators and any
            // source character from U+0020 upwards. Other C0 controls are as invalid here as
            // they are in a single-line string.
            if (ch as u32) < 0x20 && ch != '\t' && ch != '\n' && ch != '\r' {
                return Err(self.error(
                    self.location(),
                    format!("Invalid character within String: {}.", print_code_point(ch)),
                ));
            }
            if let Some(ch) = self.bump() {
                raw.push(ch);
            }
        }

        Ok(TokenKind::BlockString(Cow::Owned(block_string_value(&raw))))
    }

    /// Build a syntax error at a location.
    fn error(&self, location: Location, message: String) -> GraphQLError {
        GraphQLError::new(format!("Syntax Error: {message}")).at(location)
    }
}

/// Whether a character may start a name.
fn is_name_start(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphabetic()
}

/// Whether a byte may start a name.
#[inline]
fn is_name_start_byte(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphabetic()
}

/// Whether a byte may continue a name.
///
/// A table replaces the two range checks the predicate would otherwise do per byte, and names are
/// scanned for every token in a document.
#[inline]
fn is_name_continue_byte(byte: u8) -> bool {
    NAME_CONTINUE[byte as usize]
}

/// Which bytes may continue a name, indexed by byte value.
static NAME_CONTINUE: [bool; 256] = {
    let mut table = [false; 256];
    let mut byte = 0usize;
    while byte < 256 {
        table[byte] = matches!(byte as u8, b'_' | b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z');
        byte += 1;
    }
    table
};

/// The class of an ignored byte, used to skip whitespace runs in one step.
const IGNORED_NONE: u8 = 0;
/// A space, tab or comma, all of which only advance the column.
const IGNORED_PLAIN: u8 = 1;
const IGNORED_NEWLINE: u8 = 2;
const IGNORED_CARRIAGE_RETURN: u8 = 3;
const IGNORED_COMMENT: u8 = 4;

/// Classify one byte for [`Lexer::skip_ignored`].
#[inline]
fn ignored_class(byte: u8) -> u8 {
    IGNORED_CLASS[byte as usize]
}

/// The ignored-character class of every byte value.
static IGNORED_CLASS: [u8; 256] = {
    let mut table = [IGNORED_NONE; 256];
    table[b' ' as usize] = IGNORED_PLAIN;
    table[b'\t' as usize] = IGNORED_PLAIN;
    table[b',' as usize] = IGNORED_PLAIN;
    table[b'\n' as usize] = IGNORED_NEWLINE;
    table[b'\r' as usize] = IGNORED_CARRIAGE_RETURN;
    table[b'#' as usize] = IGNORED_COMMENT;
    table
};

/// Render a code point the way the reference implementation does in messages.
fn print_code_point(ch: char) -> String {
    let code = ch as u32;
    if (0x20..=0x7e).contains(&code) {
        format!("\"{ch}\"")
    } else {
        format!("U+{code:04X}")
    }
}

/// Apply the block string dedent algorithm from the specification.
fn block_string_value(raw: &str) -> String {
    let lines: Vec<&str> = raw.split('\n').collect();

    let mut common_indent: Option<usize> = None;
    for line in lines.iter().skip(1) {
        let length = line.chars().count();
        let indent = leading_whitespace(line);
        if indent < length {
            common_indent = Some(match common_indent {
                Some(current) => current.min(indent),
                None => indent,
            });
        }
    }

    let mut start = 0;
    let mut end = lines.len();
    while start < end && is_blank(lines.get(start).copied().unwrap_or("")) {
        start = start.saturating_add(1);
    }
    while end > start && is_blank(lines.get(end.saturating_sub(1)).copied().unwrap_or("")) {
        end = end.saturating_sub(1);
    }

    let indent = common_indent.unwrap_or(0);
    let mut value = String::new();
    for (offset, line) in lines.get(start..end).unwrap_or(&[]).iter().enumerate() {
        if offset != 0 {
            value.push('\n');
        }
        let original = start.saturating_add(offset);
        let dedented = if original == 0 {
            *line
        } else {
            skip_chars(line, indent)
        };
        value.push_str(dedented);
    }
    value
}

/// Count leading spaces and tabs.
fn leading_whitespace(line: &str) -> usize {
    line.chars()
        .take_while(|ch| *ch == ' ' || *ch == '\t')
        .count()
}

/// Whether a line contains only spaces and tabs.
fn is_blank(line: &str) -> bool {
    line.chars().all(|ch| ch == ' ' || ch == '\t')
}

/// Skip a number of characters from the start of a string.
fn skip_chars(value: &str, count: usize) -> &str {
    value
        .char_indices()
        .nth(count)
        .map_or("", |(index, _)| value.get(index..).unwrap_or(""))
}
