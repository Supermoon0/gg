//! Hand-written ES tokenizer.
//!
//! Produces a flat token stream with line numbers and newline-before
//! flags (for ASI later). The classic `/` ambiguity (regex vs divide)
//! is resolved from the previous significant token.

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum TplElem {
    /// Literal chunk with escapes already applied.
    Chunk(String),
    /// Raw source of a `${...}` hole; the parser re-lexes it.
    ExprSrc(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Num(f64),
    Str(String),
    Template(Vec<TplElem>),
    Ident(String),
    Regex { pattern: String, flags: String },
    Punct(P),
    Eof,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum P {
    LParen, RParen, LBrace, RBrace, LBracket, RBracket,
    Semi, Comma, Colon, Dot, DotDotDot, Arrow,
    Question, QuestionDot, QuestionQuestion, QuestionQuestionEq,
    Assign, EqEq, EqEqEq, NotEq, NotEqEq,
    Plus, Minus, Star, Slash, Percent, StarStar,
    PlusEq, MinusEq, StarEq, SlashEq, PercentEq, StarStarEq,
    PlusPlus, MinusMinus,
    Lt, Gt, LtEq, GtEq,
    Shl, Shr, UShr, ShlEq, ShrEq, UShrEq,
    Amp, Pipe, Caret, AmpEq, PipeEq, CaretEq,
    AmpAmp, PipePipe, AmpAmpEq, PipePipeEq,
    Not, Tilde,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: Tok,
    pub line: u32,
    pub nl_before: bool,
}

pub struct LexError {
    pub msg: String,
    pub line: u32,
}

impl fmt::Debug for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.msg)
    }
}

/// Keywords after which `/` starts a regex, not a division.
fn ident_allows_regex(s: &str) -> bool {
    matches!(s, "return" | "typeof" | "instanceof" | "in" | "of" | "new"
        | "delete" | "void" | "throw" | "case" | "do" | "else"
        | "yield" | "await")
}

pub fn tokenize(src: &str) -> Result<Vec<Token>, LexError> {
    let mut lx = Lexer {
        src,
        bytes: src.as_bytes(),
        pos: 0,
        line: 1,
        nl_before: false,
        regex_ok: true,
    };
    let mut out = Vec::new();
    loop {
        let tok = lx.next_token()?;
        let done = tok.kind == Tok::Eof;
        out.push(tok);
        if done {
            return Ok(out);
        }
    }
}

struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    line: u32,
    nl_before: bool,
    /// Whether a `/` at the current position starts a regex literal.
    regex_ok: bool,
}

impl<'a> Lexer<'a> {
    fn err(&self, msg: impl Into<String>) -> LexError {
        LexError { msg: msg.into(), line: self.line }
    }

    fn peek(&self, off: usize) -> u8 {
        *self.bytes.get(self.pos + off).unwrap_or(&0)
    }

    fn skip_ws_and_comments(&mut self) -> Result<(), LexError> {
        loop {
            match self.peek(0) {
                b' ' | b'\t' | b'\r' => self.pos += 1,
                b'\n' => {
                    self.pos += 1;
                    self.line += 1;
                    self.nl_before = true;
                }
                b'/' if self.peek(1) == b'/' => {
                    while self.pos < self.bytes.len() && self.peek(0) != b'\n' {
                        self.pos += 1;
                    }
                }
                b'/' if self.peek(1) == b'*' => {
                    self.pos += 2;
                    loop {
                        if self.pos >= self.bytes.len() {
                            return Err(self.err("unterminated block comment"));
                        }
                        if self.peek(0) == b'\n' {
                            self.line += 1;
                            self.nl_before = true;
                        }
                        if self.peek(0) == b'*' && self.peek(1) == b'/' {
                            self.pos += 2;
                            break;
                        }
                        self.pos += 1;
                    }
                }
                c if c >= 0x80 => {
                    // Unicode whitespace (NBSP, BOM/ZWNBSP, U+2028/29...)
                    let ch = self.char_here()?;
                    if ch == '\u{2028}' || ch == '\u{2029}' {
                        self.pos += ch.len_utf8();
                        self.line += 1;
                        self.nl_before = true;
                    } else if ch.is_whitespace() || ch == '\u{feff}' {
                        self.pos += ch.len_utf8();
                    } else {
                        return Ok(());
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    fn next_token(&mut self) -> Result<Token, LexError> {
        self.skip_ws_and_comments()?;
        let nl_before = self.nl_before;
        self.nl_before = false;
        let line = self.line;

        let kind = if self.pos >= self.bytes.len() {
            Tok::Eof
        } else {
            let c = self.peek(0);
            if c.is_ascii_digit()
                || (c == b'.' && self.peek(1).is_ascii_digit())
            {
                self.lex_number()?
            } else if c == b'"' || c == b'\'' {
                self.lex_string(c)?
            } else if c == b'`' {
                self.lex_template()?
            } else if is_ident_start(c) {
                self.lex_ident()?
            } else if c >= 0x80 {
                // Non-ASCII: identifier if alphanumeric, else a clean
                // error — never stall without consuming input.
                let ch = self.char_here()?;
                if ch.is_alphanumeric() {
                    self.lex_ident()?
                } else {
                    return Err(self.err(format!(
                        "unexpected character U+{:04X}", ch as u32
                    )));
                }
            } else if c == b'/' && self.regex_ok {
                self.lex_regex()?
            } else {
                self.lex_punct()?
            }
        };

        self.regex_ok = match &kind {
            Tok::Num(_) | Tok::Str(_) | Tok::Template(_)
            | Tok::Regex { .. } => false,
            Tok::Ident(name) => ident_allows_regex(name),
            Tok::Punct(p) => !matches!(
                p, P::RParen | P::RBracket | P::PlusPlus | P::MinusMinus
            ),
            Tok::Eof => false,
        };
        Ok(Token { kind, line, nl_before })
    }

    fn lex_number(&mut self) -> Result<Tok, LexError> {
        let start = self.pos;
        if self.peek(0) == b'0'
            && matches!(self.peek(1) | 0x20, b'x' | b'o' | b'b')
        {
            let radix = match self.peek(1) | 0x20 {
                b'x' => 16,
                b'o' => 8,
                _ => 2,
            };
            self.pos += 2;
            let digits = self.pos;
            while self.peek(0).is_ascii_alphanumeric() {
                self.pos += 1;
            }
            let text = &self.src[digits..self.pos];
            let n = u64::from_str_radix(text, radix)
                .map_err(|_| self.err(format!("bad number literal: {text}")))?;
            return Ok(Tok::Num(n as f64));
        }
        while self.peek(0).is_ascii_digit() {
            self.pos += 1;
        }
        if self.peek(0) == b'.' {
            self.pos += 1;
            while self.peek(0).is_ascii_digit() {
                self.pos += 1;
            }
        }
        if matches!(self.peek(0) | 0x20, b'e') {
            let mut ahead = 1;
            if matches!(self.peek(1), b'+' | b'-') {
                ahead = 2;
            }
            if self.peek(ahead).is_ascii_digit() {
                self.pos += ahead;
                while self.peek(0).is_ascii_digit() {
                    self.pos += 1;
                }
            }
        }
        let text = &self.src[start..self.pos];
        text.parse::<f64>()
            .map(Tok::Num)
            .map_err(|_| self.err(format!("bad number literal: {text}")))
    }

    /// Shared escape handling for strings and template chunks.
    fn push_escape(&mut self, out: &mut String) -> Result<(), LexError> {
        self.pos += 1;
        let e = self.peek(0);
        self.pos += 1;
        match e {
            b'n' => out.push('\n'),
            b't' => out.push('\t'),
            b'r' => out.push('\r'),
            b'b' => out.push('\u{8}'),
            b'f' => out.push('\u{c}'),
            b'v' => out.push('\u{b}'),
            b'0' if !self.peek(0).is_ascii_digit() => out.push('\0'),
            b'\n' => {
                self.line += 1;
            }
            b'x' => {
                let hex = self
                    .src
                    .get(self.pos..self.pos + 2)
                    .ok_or_else(|| self.err("bad \\x escape"))?;
                let n = u32::from_str_radix(hex, 16)
                    .map_err(|_| self.err("bad \\x escape"))?;
                out.push(char::from_u32(n).unwrap());
                self.pos += 2;
            }
            b'u' => {
                let mut n = self.lex_unicode_escape()?;
                if n > 0x10FFFF {
                    return Err(self.err("bad \\u escape"));
                }
                if (0xD800..=0xDBFF).contains(&n) {
                    // UTF-16 surrogate pair split across two escapes
                    // ("😀" = U+1F600): combine into one
                    // code point, as JS string semantics require
                    let save = self.pos;
                    let lo = if self.peek(0) == b'\\'
                        && self.peek(1) == b'u'
                    {
                        self.pos += 2;
                        self.lex_unicode_escape().ok()
                    } else {
                        None
                    };
                    match lo {
                        Some(lo) if (0xDC00..=0xDFFF).contains(&lo) => {
                            n = 0x10000
                                + ((n - 0xD800) << 10)
                                + (lo - 0xDC00);
                        }
                        _ => {
                            // lone high surrogate: a Rust String can't
                            // hold it, so substitute U+FFFD instead of
                            // failing the whole script
                            self.pos = save;
                            n = 0xFFFD;
                        }
                    }
                }
                // lone low surrogates also become U+FFFD
                out.push(char::from_u32(n).unwrap_or('\u{FFFD}'));
            }
            _ => {
                // identity escape: back up so multibyte chars decode whole
                self.pos -= 1;
                let ch = self.char_here()?;
                out.push(ch);
                self.pos += ch.len_utf8();
            }
        }
        Ok(())
    }

    /// Parse the digits of a \u escape (`{...}` or exactly 4 hex),
    /// with self.pos just past the `u`. Returns the raw code unit.
    fn lex_unicode_escape(&mut self) -> Result<u32, LexError> {
        if self.peek(0) == b'{' {
            let close = self.src[self.pos..]
                .find('}')
                .ok_or_else(|| self.err("bad \\u{} escape"))?;
            let hex = &self.src[self.pos + 1..self.pos + close];
            self.pos += close + 1;
            u32::from_str_radix(hex, 16)
                .map_err(|_| self.err("bad \\u{} escape"))
        } else {
            let hex = self
                .src
                .get(self.pos..self.pos + 4)
                .ok_or_else(|| self.err("bad \\u escape"))?;
            self.pos += 4;
            u32::from_str_radix(hex, 16)
                .map_err(|_| self.err("bad \\u escape"))
        }
    }

    fn char_here(&self) -> Result<char, LexError> {
        self.src[self.pos..]
            .chars()
            .next()
            .ok_or_else(|| self.err("unexpected end of input"))
    }

    fn lex_string(&mut self, quote: u8) -> Result<Tok, LexError> {
        self.pos += 1;
        let mut out = String::new();
        loop {
            match self.peek(0) {
                0 => return Err(self.err("unterminated string")),
                b'\n' => return Err(self.err("newline in string literal")),
                b'\\' => self.push_escape(&mut out)?,
                c if c == quote => {
                    self.pos += 1;
                    return Ok(Tok::Str(out));
                }
                c if c < 0x80 => {
                    out.push(c as char);
                    self.pos += 1;
                }
                _ => {
                    let ch = self.char_here()?;
                    out.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
    }

    fn lex_template(&mut self) -> Result<Tok, LexError> {
        self.pos += 1;
        let mut parts = Vec::new();
        let mut chunk = String::new();
        loop {
            match self.peek(0) {
                0 => return Err(self.err("unterminated template literal")),
                b'`' => {
                    self.pos += 1;
                    if !chunk.is_empty() || parts.is_empty() {
                        parts.push(TplElem::Chunk(chunk));
                    }
                    return Ok(Tok::Template(parts));
                }
                b'\\' => self.push_escape(&mut chunk)?,
                b'\n' => {
                    self.line += 1;
                    chunk.push('\n');
                    self.pos += 1;
                }
                b'$' if self.peek(1) == b'{' => {
                    if !chunk.is_empty() {
                        parts.push(TplElem::Chunk(std::mem::take(&mut chunk)));
                    }
                    self.pos += 2;
                    parts.push(TplElem::ExprSrc(self.template_expr_src()?));
                }
                c if c < 0x80 => {
                    chunk.push(c as char);
                    self.pos += 1;
                }
                _ => {
                    let ch = self.char_here()?;
                    chunk.push(ch);
                    self.pos += ch.len_utf8();
                }
            }
        }
    }

    /// Capture the raw source of a `${...}` hole up to its matching `}`,
    /// skipping strings. (Nested templates inside a hole are not
    /// supported yet — the parser stage will replace this heuristic.)
    fn template_expr_src(&mut self) -> Result<String, LexError> {
        let start = self.pos;
        let mut depth = 1usize;
        loop {
            match self.peek(0) {
                0 => return Err(self.err("unterminated ${} in template")),
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        let src = self.src[start..self.pos].to_string();
                        self.pos += 1;
                        return Ok(src);
                    }
                }
                b'\n' => self.line += 1,
                q @ (b'"' | b'\'') => {
                    self.pos += 1;
                    loop {
                        match self.peek(0) {
                            0 => return Err(self.err("unterminated string")),
                            b'\\' => self.pos += 1,
                            c if c == q => break,
                            _ => {}
                        }
                        self.pos += 1;
                    }
                }
                _ => {}
            }
            self.pos += 1;
        }
    }

    fn lex_ident(&mut self) -> Result<Tok, LexError> {
        let start = self.pos;
        loop {
            let c = self.peek(0);
            if is_ident_continue(c) {
                self.pos += 1;
            } else if c >= 0x80 {
                let ch = self.char_here()?;
                if ch.is_alphanumeric() {
                    self.pos += ch.len_utf8();
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        if self.pos == start {
            // Never emit an empty identifier: that would return a token
            // without consuming input and loop the tokenizer forever.
            let ch = self.char_here()?;
            return Err(self.err(format!(
                "unexpected character U+{:04X}", ch as u32
            )));
        }
        Ok(Tok::Ident(self.src[start..self.pos].to_string()))
    }

    fn lex_regex(&mut self) -> Result<Tok, LexError> {
        self.pos += 1;
        let start = self.pos;
        let mut in_class = false;
        loop {
            match self.peek(0) {
                0 | b'\n' => return Err(self.err("unterminated regex")),
                b'\\' => self.pos += 1,
                b'[' => in_class = true,
                b']' => in_class = false,
                b'/' if !in_class => break,
                _ => {}
            }
            self.pos += 1;
        }
        let pattern = self.src[start..self.pos].to_string();
        self.pos += 1;
        let fstart = self.pos;
        while is_ident_continue(self.peek(0)) {
            self.pos += 1;
        }
        let flags = self.src[fstart..self.pos].to_string();
        Ok(Tok::Regex { pattern, flags })
    }

    fn lex_punct(&mut self) -> Result<Tok, LexError> {
        let (a, b, c, d) =
            (self.peek(0), self.peek(1), self.peek(2), self.peek(3));
        let (p, len) = match (a, b, c, d) {
            (b'>', b'>', b'>', b'=') => (P::UShrEq, 4),
            (b'.', b'.', b'.', _) => (P::DotDotDot, 3),
            (b'=', b'=', b'=', _) => (P::EqEqEq, 3),
            (b'!', b'=', b'=', _) => (P::NotEqEq, 3),
            (b'*', b'*', b'=', _) => (P::StarStarEq, 3),
            (b'<', b'<', b'=', _) => (P::ShlEq, 3),
            (b'>', b'>', b'=', _) => (P::ShrEq, 3),
            (b'>', b'>', b'>', _) => (P::UShr, 3),
            (b'&', b'&', b'=', _) => (P::AmpAmpEq, 3),
            (b'|', b'|', b'=', _) => (P::PipePipeEq, 3),
            (b'?', b'?', b'=', _) => (P::QuestionQuestionEq, 3),
            (b'=', b'>', _, _) => (P::Arrow, 2),
            (b'=', b'=', _, _) => (P::EqEq, 2),
            (b'!', b'=', _, _) => (P::NotEq, 2),
            (b'+', b'+', _, _) => (P::PlusPlus, 2),
            (b'-', b'-', _, _) => (P::MinusMinus, 2),
            (b'+', b'=', _, _) => (P::PlusEq, 2),
            (b'-', b'=', _, _) => (P::MinusEq, 2),
            (b'*', b'*', _, _) => (P::StarStar, 2),
            (b'*', b'=', _, _) => (P::StarEq, 2),
            (b'/', b'=', _, _) => (P::SlashEq, 2),
            (b'%', b'=', _, _) => (P::PercentEq, 2),
            (b'<', b'<', _, _) => (P::Shl, 2),
            (b'>', b'>', _, _) => (P::Shr, 2),
            (b'<', b'=', _, _) => (P::LtEq, 2),
            (b'>', b'=', _, _) => (P::GtEq, 2),
            (b'&', b'&', _, _) => (P::AmpAmp, 2),
            (b'|', b'|', _, _) => (P::PipePipe, 2),
            (b'&', b'=', _, _) => (P::AmpEq, 2),
            (b'|', b'=', _, _) => (P::PipeEq, 2),
            (b'^', b'=', _, _) => (P::CaretEq, 2),
            (b'?', b'?', _, _) => (P::QuestionQuestion, 2),
            (b'?', b'.', _, _) if !c.is_ascii_digit() => (P::QuestionDot, 2),
            (b'(', ..) => (P::LParen, 1),
            (b')', ..) => (P::RParen, 1),
            (b'{', ..) => (P::LBrace, 1),
            (b'}', ..) => (P::RBrace, 1),
            (b'[', ..) => (P::LBracket, 1),
            (b']', ..) => (P::RBracket, 1),
            (b';', ..) => (P::Semi, 1),
            (b',', ..) => (P::Comma, 1),
            (b':', ..) => (P::Colon, 1),
            (b'.', ..) => (P::Dot, 1),
            (b'?', ..) => (P::Question, 1),
            (b'=', ..) => (P::Assign, 1),
            (b'+', ..) => (P::Plus, 1),
            (b'-', ..) => (P::Minus, 1),
            (b'*', ..) => (P::Star, 1),
            (b'/', ..) => (P::Slash, 1),
            (b'%', ..) => (P::Percent, 1),
            (b'<', ..) => (P::Lt, 1),
            (b'>', ..) => (P::Gt, 1),
            (b'&', ..) => (P::Amp, 1),
            (b'|', ..) => (P::Pipe, 1),
            (b'^', ..) => (P::Caret, 1),
            (b'!', ..) => (P::Not, 1),
            (b'~', ..) => (P::Tilde, 1),
            _ => {
                return Err(
                    self.err(format!("unexpected character 0x{a:02x}"))
                )
            }
        };
        self.pos += len;
        Ok(Tok::Punct(p))
    }
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c == b'$'
}

fn is_ident_continue(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<Tok> {
        tokenize(src)
            .unwrap()
            .into_iter()
            .map(|t| t.kind)
            .collect()
    }

    fn ident(s: &str) -> Tok {
        Tok::Ident(s.to_string())
    }

    #[test]
    fn basic_statement() {
        assert_eq!(
            kinds("var x = 42;"),
            vec![ident("var"), ident("x"), Tok::Punct(P::Assign),
                 Tok::Num(42.0), Tok::Punct(P::Semi), Tok::Eof]
        );
    }

    #[test]
    fn number_forms() {
        assert_eq!(
            kinds("0xff 0b101 0o17 1e3 .5 3.14 1.5e-2"),
            vec![Tok::Num(255.0), Tok::Num(5.0), Tok::Num(15.0),
                 Tok::Num(1000.0), Tok::Num(0.5), Tok::Num(3.14),
                 Tok::Num(0.015), Tok::Eof]
        );
    }

    #[test]
    fn string_escapes() {
        assert_eq!(
            kinds(r#"'a\nb' "qA" '\x41' '\u{1F600}' '한글'"#),
            vec![Tok::Str("a\nb".into()), Tok::Str("qA".into()),
                 Tok::Str("A".into()), Tok::Str("😀".into()),
                 Tok::Str("한글".into()), Tok::Eof]
        );
    }

    #[test]
    fn surrogate_pair_escapes_combine() {
        // both the 4-hex and braced forms of a split pair
        assert_eq!(
            kinds(r#""😀" '\u{D83D}\u{DE00}' `😀`"#),
            vec![Tok::Str("😀".into()), Tok::Str("😀".into()),
                 Tok::Template(vec![TplElem::Chunk("😀".into())]),
                 Tok::Eof]
        );
    }

    #[test]
    fn lone_surrogates_do_not_fail_the_script() {
        // unpaired surrogates become U+FFFD instead of a LexError
        assert_eq!(
            kinds(r#""a\uD800b" "\uDC00" "\uD800A""#),
            vec![Tok::Str("a\u{FFFD}b".into()),
                 Tok::Str("\u{FFFD}".into()),
                 Tok::Str("\u{FFFD}A".into()), Tok::Eof]
        );
    }

    #[test]
    fn out_of_range_unicode_escape_still_errors() {
        assert!(tokenize(r#""\u{110000}""#).is_err());
    }

    #[test]
    fn punct_maximal_munch() {
        assert_eq!(
            kinds("a===b x>>>=2 f=>y o?.p n??m i++ + ++j"),
            vec![ident("a"), Tok::Punct(P::EqEqEq), ident("b"),
                 ident("x"), Tok::Punct(P::UShrEq), Tok::Num(2.0),
                 ident("f"), Tok::Punct(P::Arrow), ident("y"),
                 ident("o"), Tok::Punct(P::QuestionDot), ident("p"),
                 ident("n"), Tok::Punct(P::QuestionQuestion), ident("m"),
                 ident("i"), Tok::Punct(P::PlusPlus), Tok::Punct(P::Plus),
                 Tok::Punct(P::PlusPlus), ident("j"), Tok::Eof]
        );
    }

    #[test]
    fn ternary_with_number_is_not_optional_chain() {
        assert_eq!(
            kinds("a?.5:b"),
            vec![ident("a"), Tok::Punct(P::Question), Tok::Num(0.5),
                 Tok::Punct(P::Colon), ident("b"), Tok::Eof]
        );
    }

    #[test]
    fn comments_and_newline_flags() {
        let toks = tokenize("a // hi\nb /* x\ny */ c").unwrap();
        let names: Vec<_> = toks.iter().map(|t| &t.kind).collect();
        assert_eq!(
            names,
            vec![&ident("a"), &ident("b"), &ident("c"), &Tok::Eof]
        );
        assert!(!toks[0].nl_before && toks[1].nl_before && toks[2].nl_before);
        assert_eq!((toks[0].line, toks[1].line, toks[2].line), (1, 2, 3));
    }

    #[test]
    fn non_ascii_never_stalls() {
        // NBSP / BOM / ideographic space are whitespace between tokens
        assert_eq!(
            kinds("a\u{a0}b\u{feff}c\u{3000}d"),
            vec![ident("a"), ident("b"), ident("c"), ident("d"), Tok::Eof]
        );
        // U+2028/29 are line terminators: bump line, set nl_before (ASI)
        let toks = tokenize("a\u{2028}b").unwrap();
        assert!(toks[1].nl_before);
        assert_eq!(toks[1].line, 2);
        // Unicode identifiers still lex
        assert_eq!(kinds("한글 = 1")[0], ident("한글"));
        // Anything else errors instead of looping forever
        assert!(tokenize("a → b").is_err());
        assert!(tokenize("let x = 1; ©").is_err());
    }

    #[test]
    fn regex_vs_division() {
        assert_eq!(
            kinds("a / b / c"),
            vec![ident("a"), Tok::Punct(P::Slash), ident("b"),
                 Tok::Punct(P::Slash), ident("c"), Tok::Eof]
        );
        assert_eq!(
            kinds("x = /ab+c/gi;"),
            vec![ident("x"), Tok::Punct(P::Assign),
                 Tok::Regex { pattern: "ab+c".into(), flags: "gi".into() },
                 Tok::Punct(P::Semi), Tok::Eof]
        );
        assert_eq!(
            kinds("return /x/;"),
            vec![ident("return"),
                 Tok::Regex { pattern: "x".into(), flags: "".into() },
                 Tok::Punct(P::Semi), Tok::Eof]
        );
        assert_eq!(
            kinds("(a) / 2"),
            vec![Tok::Punct(P::LParen), ident("a"), Tok::Punct(P::RParen),
                 Tok::Punct(P::Slash), Tok::Num(2.0), Tok::Eof]
        );
        assert_eq!(
            kinds("/[/]/"),
            vec![Tok::Regex { pattern: "[/]".into(), flags: "".into() },
                 Tok::Eof]
        );
    }

    #[test]
    fn template_literal() {
        assert_eq!(
            kinds("`hi ${name}! ${a + \"}\"} end`"),
            vec![Tok::Template(vec![
                TplElem::Chunk("hi ".into()),
                TplElem::ExprSrc("name".into()),
                TplElem::Chunk("! ".into()),
                TplElem::ExprSrc("a + \"}\"".into()),
                TplElem::Chunk(" end".into()),
            ]), Tok::Eof]
        );
        assert_eq!(kinds("``"), vec![Tok::Template(vec![
            TplElem::Chunk("".into())]), Tok::Eof]);
    }
}
