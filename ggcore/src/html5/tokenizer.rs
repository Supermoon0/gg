//! The HTML5 tokenizer, following the WHATWG spec's state machine.
//!
//! Written as an explicit state enum driven one code point at a time,
//! the same shape the spec is written in, so a state here can be read
//! against the section that defines it. The tree builder pulls tokens
//! and can push the tokenizer into RCDATA/RAWTEXT/script/PLAINTEXT
//! states, which is how the spec threads element context back into
//! lexing.

use super::entities::ENTITIES;
use super::sink::AttrName;

#[derive(Clone, Debug, PartialEq)]
pub enum Token {
    Doctype {
        name: Option<String>,
        public_id: Option<String>,
        system_id: Option<String>,
        force_quirks: bool,
    },
    StartTag {
        name: String,
        attrs: Vec<(AttrName, String)>,
        self_closing: bool,
    },
    EndTag {
        name: String,
    },
    /// A run of adjacent character tokens. The spec emits one token per
    /// character; a page whose bulk is an inline <script> is then two
    /// hundred thousand tokens, and the queue traffic costs more than
    /// the tokenizing. Adjacent characters coalesce here and the tree
    /// builder unpacks them one at a time, so the insertion modes still
    /// see the spec's stream.
    Chars(String),
    Comment(String),
    Char(char),
    Eof,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Data,
    Rcdata,
    Rawtext,
    ScriptData,
    Plaintext,
    TagOpen,
    EndTagOpen,
    TagName,
    RcdataLessThan,
    RcdataEndTagOpen,
    RcdataEndTagName,
    RawtextLessThan,
    RawtextEndTagOpen,
    RawtextEndTagName,
    ScriptDataLessThan,
    ScriptDataEndTagOpen,
    ScriptDataEndTagName,
    ScriptDataEscapeStart,
    ScriptDataEscapeStartDash,
    ScriptDataEscaped,
    ScriptDataEscapedDash,
    ScriptDataEscapedDashDash,
    ScriptDataEscapedLessThan,
    ScriptDataEscapedEndTagOpen,
    ScriptDataEscapedEndTagName,
    ScriptDataDoubleEscapeStart,
    ScriptDataDoubleEscaped,
    ScriptDataDoubleEscapedDash,
    ScriptDataDoubleEscapedDashDash,
    ScriptDataDoubleEscapedLessThan,
    ScriptDataDoubleEscapeEnd,
    BeforeAttributeName,
    AttributeName,
    AfterAttributeName,
    BeforeAttributeValue,
    AttributeValueDouble,
    AttributeValueSingle,
    AttributeValueUnquoted,
    AfterAttributeValueQuoted,
    SelfClosingStartTag,
    BogusComment,
    MarkupDeclarationOpen,
    CommentStart,
    CommentStartDash,
    Comment,
    CommentLessThan,
    CommentLessThanBang,
    CommentLessThanBangDash,
    CommentLessThanBangDashDash,
    CommentEndDash,
    CommentEnd,
    CommentEndBang,
    Doctype,
    BeforeDoctypeName,
    DoctypeName,
    AfterDoctypeName,
    AfterDoctypePublicKeyword,
    BeforeDoctypePublicIdentifier,
    DoctypePublicIdentifierDouble,
    DoctypePublicIdentifierSingle,
    AfterDoctypePublicIdentifier,
    BetweenDoctypePublicAndSystem,
    AfterDoctypeSystemKeyword,
    BeforeDoctypeSystemIdentifier,
    DoctypeSystemIdentifierDouble,
    DoctypeSystemIdentifierSingle,
    AfterDoctypeSystemIdentifier,
    BogusDoctype,
    CdataSection,
    CdataSectionBracket,
    CdataSectionEnd,
}

pub struct Tokenizer {
    input: String,
    pos: usize,
    pub state: State,
    return_state: State,
    /// Tokens produced this step; the tree builder drains them.
    pub out: Vec<Token>,
    // in-progress token scratch
    tag_name: String,
    tag_is_end: bool,
    self_closing: bool,
    attrs: Vec<(AttrName, String)>,
    attr_name: String,
    attr_value: String,
    in_attr: bool,
    comment: String,
    dt_name: Option<String>,
    dt_public: Option<String>,
    dt_system: Option<String>,
    dt_force_quirks: bool,
    /// The last start tag name emitted — an end tag only closes
    /// RCDATA/RAWTEXT/script when it matches this.
    last_start_tag: String,
    temp: String,
    char_ref_code: u32,
    /// true once the tokenizer has run off the end of the input
    pub eof: bool,
    consumed: bool,
    /// Bytes the last `next` took, so `reconsume` can step back over a
    /// character it never had to decode twice.
    last_len: usize,
    /// Whether a `<![CDATA[` here is real CDATA. The tree builder sets
    /// this from the adjusted current node's namespace before each
    /// token pull; outside foreign content the spec makes it a bogus
    /// comment instead.
    pub cdata_ok: bool,
}

fn is_ws(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\u{0c}' | ' ')
}

impl Tokenizer {
    pub fn new(input: &str) -> Tokenizer {
        // Preprocessing: normalize newlines, per the spec's input
        // stream preprocessing step.
        // Only a carriage return needs rewriting, and most documents
        // have none: scanning for one is far cheaper than copying the
        // whole input a character at a time to find out.
        let chars = match input.find('\r') {
            None => input.to_string(),
            Some(first) => {
                let mut out = String::with_capacity(input.len());
                let mut rest = input;
                let mut at = first;
                loop {
                    out.push_str(&rest[..at]);
                    out.push('\n');
                    rest = &rest[at + 1..];
                    if let Some(s) = rest.strip_prefix('\n') {
                        rest = s;
                    }
                    match rest.find('\r') {
                        Some(next) => at = next,
                        None => break,
                    }
                }
                out.push_str(rest);
                out
            }
        };
        Tokenizer {
            input: chars,
            pos: 0,
            state: State::Data,
            return_state: State::Data,
            out: Vec::new(),
            tag_name: String::new(),
            tag_is_end: false,
            self_closing: false,
            attrs: Vec::new(),
            attr_name: String::new(),
            attr_value: String::new(),
            in_attr: false,
            comment: String::new(),
            dt_name: None,
            dt_public: None,
            dt_system: None,
            dt_force_quirks: false,
            last_start_tag: String::new(),
            temp: String::new(),
            char_ref_code: 0,
            eof: false,
            cdata_ok: false,
            consumed: false,
            last_len: 0,
        }
    }

    /// Re-feed markup at the current position — `document.write` during
    /// parsing inserts at the insertion point.
    pub fn insert_at_point(&mut self, text: &str) {
        self.input.insert_str(self.pos, text);
    }

    /// Markup is overwhelmingly ASCII, and slicing a `str` costs a
    /// char-boundary check that a byte load does not.
    fn next(&mut self) -> Option<char> {
        match self.input.as_bytes().get(self.pos) {
            None => {
                self.consumed = false;
                None
            }
            Some(&b) if b < 0x80 => {
                self.consumed = true;
                self.last_len = 1;
                self.pos += 1;
                Some(b as char)
            }
            Some(_) => {
                let c = self.input[self.pos..].chars().next()?;
                self.consumed = true;
                self.last_len = c.len_utf8();
                self.pos += self.last_len;
                Some(c)
            }
        }
    }

    fn peek(&self) -> Option<char> {
        match self.input.as_bytes().get(self.pos) {
            None => None,
            Some(&b) if b < 0x80 => Some(b as char),
            Some(_) => self.input[self.pos..].chars().next(),
        }
    }

    /// The spec's "reconsume": put back the character the current state
    /// just took. At EOF the state consumed nothing, so there is nothing
    /// to put back — stepping back anyway would re-read the last real
    /// character and lose it from the output.
    fn reconsume(&mut self) {
        if self.consumed {
            self.pos -= self.last_len;
            self.consumed = false;
        }
    }

    /// Case-insensitive lookahead used by the few states the spec
    /// defines in terms of matching a literal string.
    fn match_ahead_ci(&mut self, s: &str) -> bool {
        // Every literal the spec matches this way is ASCII, so a byte
        // comparison says the same thing a character one would.
        let end = self.pos + s.len();
        if end > self.input.len() || !self.input.is_char_boundary(end) {
            return false;
        }
        if !self.input[self.pos..end].eq_ignore_ascii_case(s) {
            return false;
        }
        self.pos = end;
        true
    }

    fn match_ahead(&mut self, s: &str) -> bool {
        let end = self.pos + s.len();
        if end > self.input.len() || !self.input.is_char_boundary(end) {
            return false;
        }
        if &self.input[self.pos..end] != s {
            return false;
        }
        self.pos = end;
        true
    }

    fn emit(&mut self, t: Token) {
        if let Token::StartTag { name, .. } = &t {
            self.last_start_tag = name.clone();
        }
        self.out.push(t);
    }

    fn emit_char(&mut self, c: char) {
        if let Some(Token::Chars(run)) = self.out.last_mut() {
            run.push(c);
            return;
        }
        self.out.push(Token::Chars(c.to_string()));
    }

    fn emit_str(&mut self, s: &str) {
        if let Some(Token::Chars(run)) = self.out.last_mut() {
            run.push_str(s);
            return;
        }
        self.out.push(Token::Chars(s.to_string()));
    }

    fn start_tag(&mut self, is_end: bool) {
        self.tag_name.clear();
        self.tag_is_end = is_end;
        self.self_closing = false;
        self.attrs.clear();
        self.in_attr = false;
    }

    fn finish_attr(&mut self) {
        if !self.in_attr {
            return;
        }
        self.in_attr = false;
        let name = std::mem::take(&mut self.attr_name);
        let value = std::mem::take(&mut self.attr_value);
        // duplicate attribute: the first one wins
        if !self.attrs.iter().any(|(k, _)| k.prefix.is_none() && k.local == name)
        {
            self.attrs.push((AttrName::local(name), value));
        }
    }

    fn emit_tag(&mut self) {
        self.finish_attr();
        let name = std::mem::take(&mut self.tag_name);
        if self.tag_is_end {
            self.emit(Token::EndTag { name });
        } else {
            let attrs = std::mem::take(&mut self.attrs);
            let sc = self.self_closing;
            self.emit(Token::StartTag { name, attrs, self_closing: sc });
        }
    }

    fn emit_comment(&mut self) {
        let c = std::mem::take(&mut self.comment);
        self.emit(Token::Comment(c));
    }

    fn emit_doctype(&mut self) {
        let t = Token::Doctype {
            name: self.dt_name.take(),
            public_id: self.dt_public.take(),
            system_id: self.dt_system.take(),
            force_quirks: self.dt_force_quirks,
        };
        self.dt_force_quirks = false;
        self.emit(t);
    }

    /// Is the end tag being built the one that can close the current
    /// RCDATA/RAWTEXT/script element?
    fn appropriate_end_tag(&self) -> bool {
        self.tag_name == self.last_start_tag
    }

    /// Run until at least one token is available (or EOF), then hand
    /// them over in `into`. The caller's buffer comes back to us so the
    /// allocation is made once for the whole document rather than once
    /// per token — script data emits a token per character, so that is
    /// the difference between one allocation and two hundred thousand.
    pub fn next_tokens_into(&mut self, into: &mut Vec<Token>) {
        // Keep stepping while all we have is a character run: the run
        // grows in place, and only a real token ends the batch. A tag
        // is the last thing in the queue when it arrives, so the tree
        // builder still gets to set the tokenizer's state before the
        // next character is read.
        while !self.eof
            && self.out.last().is_none_or(|t| matches!(t, Token::Chars(_)))
        {
            self.step();
        }
        into.clear();
        std::mem::swap(into, &mut self.out);
    }

    /// Consume every character up to the next one this state has to
    /// think about, and emit the lot as one run. The spec's model is a
    /// character at a time; taking that literally means a round trip
    /// through the dispatcher for each byte of a 200 KB inline script,
    /// which costs far more than the tokenizing does. Returns whether
    /// anything was consumed.
    /// Where a run of characters the current state can take verbatim
    /// ends. Every stop byte is ASCII, and a UTF-8 continuation byte is
    /// never ASCII, so scanning bytes cannot stop mid-character.
    fn run_end(&self, stop: &[u8]) -> usize {
        let bytes = self.input.as_bytes();
        let mut i = self.pos;
        while i < bytes.len() && !stop.contains(&bytes[i]) {
            i += 1;
        }
        i
    }

    /// The same run, appended to an attribute value. Attribute values
    /// are where most of a markup-heavy page's bytes live, and they are
    /// copied through unchanged.
    fn take_attr_run(&mut self, stop: &[u8]) -> bool {
        let end = self.run_end(stop);
        if end == self.pos {
            return false;
        }
        self.attr_value.push_str(&self.input[self.pos..end]);
        self.pos = end;
        self.consumed = false;
        true
    }

    /// A run appended to a tag or attribute name, lowercased in place
    /// rather than a character at a time.
    fn take_name_run(&mut self, stop: &[u8], attr: bool) -> bool {
        let end = self.run_end(stop);
        if end == self.pos {
            return false;
        }
        let dst =
            if attr { &mut self.attr_name } else { &mut self.tag_name };
        let at = dst.len();
        dst.push_str(&self.input[self.pos..end]);
        let dst =
            if attr { &mut self.attr_name } else { &mut self.tag_name };
        dst[at..].make_ascii_lowercase();
        self.pos = end;
        self.consumed = false;
        true
    }

    fn take_comment_run(&mut self) -> bool {
        let end = self.run_end(b"<-\0");
        if end == self.pos {
            return false;
        }
        self.comment.push_str(&self.input[self.pos..end]);
        self.pos = end;
        self.consumed = false;
        true
    }

    fn take_run(&mut self, stop: &[u8]) -> bool {
        let start = self.pos;
        let bytes = self.input.as_bytes();
        // Every stop byte is ASCII, and a UTF-8 continuation byte is
        // never ASCII, so scanning bytes cannot stop mid-character.
        while self.pos < bytes.len() && !stop.contains(&bytes[self.pos]) {
            self.pos += 1;
        }
        if self.pos == start {
            return false;
        }
        if !matches!(self.out.last(), Some(Token::Chars(_))) {
            self.out.push(Token::Chars(String::new()));
        }
        let (from, to) = (start, self.pos);
        if let Some(Token::Chars(run)) = self.out.last_mut() {
            run.push_str(&self.input[from..to]);
        }
        self.consumed = false;
        true
    }

    /// The states whose bulk is ordinary character data, and the
    /// characters each of them has to stop and think about.
    fn bulk_stop(&self) -> &'static [u8] {
        use State::*;
        match self.state {
            // a NULL in Data is emitted unchanged, so it can ride along
            Data => b"&<",
            Rcdata => b"&<\0",
            Rawtext | ScriptData => b"<\0",
            Plaintext => b"\0",
            _ => &[],
        }
    }

    fn step(&mut self) {
        use State::*;
        let stop = self.bulk_stop();
        if !stop.is_empty() && self.take_run(stop) {
            return;
        }
        match self.state {
            Data => match self.next() {
                Some('&') => {
                    self.return_state = Data;
                    self.char_reference();
                }
                Some('<') => self.state = TagOpen,
                Some('\0') => self.emit_char('\0'),
                Some(c) => self.emit_char(c),
                None => self.finish(),
            },
            Rcdata => match self.next() {
                Some('&') => {
                    self.return_state = Rcdata;
                    self.char_reference();
                }
                Some('<') => self.state = RcdataLessThan,
                Some('\0') => self.emit_char('\u{fffd}'),
                Some(c) => self.emit_char(c),
                None => self.finish(),
            },
            Rawtext => match self.next() {
                Some('<') => self.state = RawtextLessThan,
                Some('\0') => self.emit_char('\u{fffd}'),
                Some(c) => self.emit_char(c),
                None => self.finish(),
            },
            ScriptData => match self.next() {
                Some('<') => self.state = ScriptDataLessThan,
                Some('\0') => self.emit_char('\u{fffd}'),
                Some(c) => self.emit_char(c),
                None => self.finish(),
            },
            Plaintext => match self.next() {
                Some('\0') => self.emit_char('\u{fffd}'),
                Some(c) => self.emit_char(c),
                None => self.finish(),
            },
            TagOpen => match self.next() {
                Some('!') => self.state = MarkupDeclarationOpen,
                Some('/') => self.state = EndTagOpen,
                Some(c) if c.is_ascii_alphabetic() => {
                    self.start_tag(false);
                    self.reconsume();
                    self.state = TagName;
                }
                Some('?') => {
                    self.comment.clear();
                    self.reconsume();
                    self.state = BogusComment;
                }
                Some(_) => {
                    self.emit_char('<');
                    self.reconsume();
                    self.state = Data;
                }
                None => {
                    self.emit_char('<');
                    self.finish();
                }
            },
            EndTagOpen => match self.next() {
                Some(c) if c.is_ascii_alphabetic() => {
                    self.start_tag(true);
                    self.reconsume();
                    self.state = TagName;
                }
                Some('>') => self.state = Data,
                Some(_) => {
                    self.comment.clear();
                    self.reconsume();
                    self.state = BogusComment;
                }
                None => {
                    self.emit_char('<');
                    self.emit_char('/');
                    self.finish();
                }
            },
            TagName => {
                if self.take_name_run(b" \t\n\x0c/>\0", false) {
                    return;
                }
                match self.next() {
                Some(c) if is_ws(c) => self.state = BeforeAttributeName,
                Some('/') => self.state = SelfClosingStartTag,
                Some('>') => {
                    self.emit_tag();
                    self.state = Data;
                }
                Some('\0') => self.tag_name.push('\u{fffd}'),
                Some(c) => self.tag_name.push(c.to_ascii_lowercase()),
                None => self.finish(),
                }
            }

            // --- RCDATA / RAWTEXT / script "less-than" families ---
            RcdataLessThan => match self.next() {
                Some('/') => {
                    self.temp.clear();
                    self.state = RcdataEndTagOpen;
                }
                _ => {
                    self.reconsume();
                    self.emit_char('<');
                    self.state = Rcdata;
                }
            },
            RcdataEndTagOpen => match self.next() {
                Some(c) if c.is_ascii_alphabetic() => {
                    self.start_tag(true);
                    self.reconsume();
                    self.state = RcdataEndTagName;
                }
                _ => {
                    self.reconsume();
                    self.emit_str("</");
                    self.state = Rcdata;
                }
            },
            RcdataEndTagName => {
                self.end_tag_name_state(Rcdata);
            }
            RawtextLessThan => match self.next() {
                Some('/') => {
                    self.temp.clear();
                    self.state = RawtextEndTagOpen;
                }
                _ => {
                    self.reconsume();
                    self.emit_char('<');
                    self.state = Rawtext;
                }
            },
            RawtextEndTagOpen => match self.next() {
                Some(c) if c.is_ascii_alphabetic() => {
                    self.start_tag(true);
                    self.reconsume();
                    self.state = RawtextEndTagName;
                }
                _ => {
                    self.reconsume();
                    self.emit_str("</");
                    self.state = Rawtext;
                }
            },
            RawtextEndTagName => {
                self.end_tag_name_state(Rawtext);
            }
            ScriptDataLessThan => match self.next() {
                Some('/') => {
                    self.temp.clear();
                    self.state = ScriptDataEndTagOpen;
                }
                Some('!') => {
                    self.emit_str("<!");
                    self.state = ScriptDataEscapeStart;
                }
                _ => {
                    self.reconsume();
                    self.emit_char('<');
                    self.state = ScriptData;
                }
            },
            ScriptDataEndTagOpen => match self.next() {
                Some(c) if c.is_ascii_alphabetic() => {
                    self.start_tag(true);
                    self.reconsume();
                    self.state = ScriptDataEndTagName;
                }
                _ => {
                    self.reconsume();
                    self.emit_str("</");
                    self.state = ScriptData;
                }
            },
            ScriptDataEndTagName => {
                self.end_tag_name_state(ScriptData);
            }
            ScriptDataEscapeStart => match self.next() {
                Some('-') => {
                    self.emit_char('-');
                    self.state = ScriptDataEscapeStartDash;
                }
                _ => {
                    self.reconsume();
                    self.state = ScriptData;
                }
            },
            ScriptDataEscapeStartDash => match self.next() {
                Some('-') => {
                    self.emit_char('-');
                    self.state = ScriptDataEscapedDashDash;
                }
                _ => {
                    self.reconsume();
                    self.state = ScriptData;
                }
            },
            ScriptDataEscaped => match self.next() {
                Some('-') => {
                    self.emit_char('-');
                    self.state = ScriptDataEscapedDash;
                }
                Some('<') => self.state = ScriptDataEscapedLessThan,
                Some('\0') => self.emit_char('\u{fffd}'),
                Some(c) => self.emit_char(c),
                None => self.finish(),
            },
            ScriptDataEscapedDash => match self.next() {
                Some('-') => {
                    self.emit_char('-');
                    self.state = ScriptDataEscapedDashDash;
                }
                Some('<') => self.state = ScriptDataEscapedLessThan,
                Some('\0') => {
                    self.emit_char('\u{fffd}');
                    self.state = ScriptDataEscaped;
                }
                Some(c) => {
                    self.emit_char(c);
                    self.state = ScriptDataEscaped;
                }
                None => self.finish(),
            },
            ScriptDataEscapedDashDash => match self.next() {
                Some('-') => self.emit_char('-'),
                Some('<') => self.state = ScriptDataEscapedLessThan,
                Some('>') => {
                    self.emit_char('>');
                    self.state = ScriptData;
                }
                Some('\0') => {
                    self.emit_char('\u{fffd}');
                    self.state = ScriptDataEscaped;
                }
                Some(c) => {
                    self.emit_char(c);
                    self.state = ScriptDataEscaped;
                }
                None => self.finish(),
            },
            ScriptDataEscapedLessThan => match self.next() {
                Some('/') => {
                    self.temp.clear();
                    self.state = ScriptDataEscapedEndTagOpen;
                }
                Some(c) if c.is_ascii_alphabetic() => {
                    self.temp.clear();
                    self.emit_char('<');
                    self.reconsume();
                    self.state = ScriptDataDoubleEscapeStart;
                }
                _ => {
                    self.reconsume();
                    self.emit_char('<');
                    self.state = ScriptDataEscaped;
                }
            },
            ScriptDataEscapedEndTagOpen => match self.next() {
                Some(c) if c.is_ascii_alphabetic() => {
                    self.start_tag(true);
                    self.reconsume();
                    self.state = ScriptDataEscapedEndTagName;
                }
                _ => {
                    self.reconsume();
                    self.emit_str("</");
                    self.state = ScriptDataEscaped;
                }
            },
            ScriptDataEscapedEndTagName => {
                self.end_tag_name_state(ScriptDataEscaped);
            }
            ScriptDataDoubleEscapeStart => match self.next() {
                Some(c) if is_ws(c) || c == '/' || c == '>' => {
                    self.emit_char(c);
                    self.state = if self.temp == "script" {
                        ScriptDataDoubleEscaped
                    } else {
                        ScriptDataEscaped
                    };
                }
                Some(c) if c.is_ascii_alphabetic() => {
                    self.temp.push(c.to_ascii_lowercase());
                    self.emit_char(c);
                }
                _ => {
                    self.reconsume();
                    self.state = ScriptDataEscaped;
                }
            },
            ScriptDataDoubleEscaped => match self.next() {
                Some('-') => {
                    self.emit_char('-');
                    self.state = ScriptDataDoubleEscapedDash;
                }
                Some('<') => {
                    self.emit_char('<');
                    self.state = ScriptDataDoubleEscapedLessThan;
                }
                Some('\0') => self.emit_char('\u{fffd}'),
                Some(c) => self.emit_char(c),
                None => self.finish(),
            },
            ScriptDataDoubleEscapedDash => match self.next() {
                Some('-') => {
                    self.emit_char('-');
                    self.state = ScriptDataDoubleEscapedDashDash;
                }
                Some('<') => {
                    self.emit_char('<');
                    self.state = ScriptDataDoubleEscapedLessThan;
                }
                Some('\0') => {
                    self.emit_char('\u{fffd}');
                    self.state = ScriptDataDoubleEscaped;
                }
                Some(c) => {
                    self.emit_char(c);
                    self.state = ScriptDataDoubleEscaped;
                }
                None => self.finish(),
            },
            ScriptDataDoubleEscapedDashDash => match self.next() {
                Some('-') => self.emit_char('-'),
                Some('<') => {
                    self.emit_char('<');
                    self.state = ScriptDataDoubleEscapedLessThan;
                }
                Some('>') => {
                    self.emit_char('>');
                    self.state = ScriptData;
                }
                Some('\0') => {
                    self.emit_char('\u{fffd}');
                    self.state = ScriptDataDoubleEscaped;
                }
                Some(c) => {
                    self.emit_char(c);
                    self.state = ScriptDataDoubleEscaped;
                }
                None => self.finish(),
            },
            ScriptDataDoubleEscapedLessThan => match self.next() {
                Some('/') => {
                    self.temp.clear();
                    self.emit_char('/');
                    self.state = ScriptDataDoubleEscapeEnd;
                }
                _ => {
                    self.reconsume();
                    self.state = ScriptDataDoubleEscaped;
                }
            },
            ScriptDataDoubleEscapeEnd => match self.next() {
                Some(c) if is_ws(c) || c == '/' || c == '>' => {
                    self.emit_char(c);
                    self.state = if self.temp == "script" {
                        ScriptDataEscaped
                    } else {
                        ScriptDataDoubleEscaped
                    };
                }
                Some(c) if c.is_ascii_alphabetic() => {
                    self.temp.push(c.to_ascii_lowercase());
                    self.emit_char(c);
                }
                _ => {
                    self.reconsume();
                    self.state = ScriptDataDoubleEscaped;
                }
            },

            // --- attributes ---
            BeforeAttributeName => match self.next() {
                Some(c) if is_ws(c) => {}
                Some('/') | Some('>') => {
                    self.reconsume();
                    self.state = AfterAttributeName;
                }
                None => {
                    self.state = AfterAttributeName;
                }
                Some('=') => {
                    self.finish_attr();
                    self.attr_name = "=".to_string();
                    self.attr_value.clear();
                    self.in_attr = true;
                    self.state = AttributeName;
                }
                Some(_) => {
                    self.finish_attr();
                    self.attr_name.clear();
                    self.attr_value.clear();
                    self.in_attr = true;
                    self.reconsume();
                    self.state = AttributeName;
                }
            },
            AttributeName => {
                if self.take_name_run(b" \t\n\x0c/>=\0", true) {
                    return;
                }
                match self.next() {
                Some(c) if is_ws(c) => {
                    self.reconsume();
                    self.state = AfterAttributeName;
                }
                Some('/') | Some('>') => {
                    self.reconsume();
                    self.state = AfterAttributeName;
                }
                None => self.state = AfterAttributeName,
                Some('=') => self.state = BeforeAttributeValue,
                Some('\0') => self.attr_name.push('\u{fffd}'),
                Some(c) => self.attr_name.push(c.to_ascii_lowercase()),
                }
            }
            AfterAttributeName => match self.next() {
                Some(c) if is_ws(c) => {}
                Some('/') => {
                    self.finish_attr();
                    self.state = SelfClosingStartTag;
                }
                Some('=') => self.state = BeforeAttributeValue,
                Some('>') => {
                    self.emit_tag();
                    self.state = Data;
                }
                None => self.finish(),
                Some(_) => {
                    self.finish_attr();
                    self.attr_name.clear();
                    self.attr_value.clear();
                    self.in_attr = true;
                    self.reconsume();
                    self.state = AttributeName;
                }
            },
            BeforeAttributeValue => match self.next() {
                Some(c) if is_ws(c) => {}
                Some('"') => self.state = AttributeValueDouble,
                Some('\'') => self.state = AttributeValueSingle,
                Some('>') => {
                    self.emit_tag();
                    self.state = Data;
                }
                _ => {
                    self.reconsume();
                    self.state = AttributeValueUnquoted;
                }
            },
            AttributeValueDouble => {
                if self.take_attr_run(b"\"&\0") {
                    return;
                }
                match self.next() {
                Some('"') => self.state = AfterAttributeValueQuoted,
                Some('&') => {
                    self.return_state = AttributeValueDouble;
                    self.char_reference();
                }
                Some('\0') => self.attr_value.push('\u{fffd}'),
                Some(c) => self.attr_value.push(c),
                None => self.finish(),
                }
            }
            AttributeValueSingle => {
                if self.take_attr_run(b"'&\0") {
                    return;
                }
                match self.next() {
                Some('\'') => self.state = AfterAttributeValueQuoted,
                Some('&') => {
                    self.return_state = AttributeValueSingle;
                    self.char_reference();
                }
                Some('\0') => self.attr_value.push('\u{fffd}'),
                Some(c) => self.attr_value.push(c),
                None => self.finish(),
                }
            }
            AttributeValueUnquoted => match self.next() {
                Some(c) if is_ws(c) => self.state = BeforeAttributeName,
                Some('&') => {
                    self.return_state = AttributeValueUnquoted;
                    self.char_reference();
                }
                Some('>') => {
                    self.emit_tag();
                    self.state = Data;
                }
                Some('\0') => self.attr_value.push('\u{fffd}'),
                Some(c) => self.attr_value.push(c),
                None => self.finish(),
            },
            AfterAttributeValueQuoted => match self.next() {
                Some(c) if is_ws(c) => self.state = BeforeAttributeName,
                Some('/') => self.state = SelfClosingStartTag,
                Some('>') => {
                    self.emit_tag();
                    self.state = Data;
                }
                None => self.finish(),
                Some(_) => {
                    self.reconsume();
                    self.state = BeforeAttributeName;
                }
            },
            SelfClosingStartTag => match self.next() {
                Some('>') => {
                    self.self_closing = true;
                    self.emit_tag();
                    self.state = Data;
                }
                None => self.finish(),
                Some(_) => {
                    self.reconsume();
                    self.state = BeforeAttributeName;
                }
            },

            // --- comments and declarations ---
            BogusComment => match self.next() {
                Some('>') => {
                    self.emit_comment();
                    self.state = Data;
                }
                Some('\0') => self.comment.push('\u{fffd}'),
                Some(c) => self.comment.push(c),
                None => {
                    self.emit_comment();
                    self.finish();
                }
            },
            MarkupDeclarationOpen => {
                if self.match_ahead("--") {
                    self.comment.clear();
                    self.state = CommentStart;
                } else if self.match_ahead_ci("DOCTYPE") {
                    self.state = Doctype;
                } else if self.match_ahead("[CDATA[") {
                    // The tree builder decides whether this is real
                    // CDATA (foreign content) or a bogus comment; it
                    // sets `cdata_ok` before we get here.
                    if self.cdata_ok {
                        self.state = CdataSection;
                    } else {
                        self.comment = "[CDATA[".to_string();
                        self.state = BogusComment;
                    }
                } else {
                    self.comment.clear();
                    self.state = BogusComment;
                }
            }
            CommentStart => match self.next() {
                Some('-') => self.state = CommentStartDash,
                Some('>') => {
                    self.emit_comment();
                    self.state = Data;
                }
                _ => {
                    self.reconsume();
                    self.state = Comment;
                }
            },
            CommentStartDash => match self.next() {
                Some('-') => self.state = CommentEnd,
                Some('>') => {
                    self.emit_comment();
                    self.state = Data;
                }
                None => {
                    self.emit_comment();
                    self.finish();
                }
                Some(_) => {
                    self.comment.push('-');
                    self.reconsume();
                    self.state = Comment;
                }
            },
            Comment => {
                if self.take_comment_run() {
                    return;
                }
                match self.next() {
                Some('<') => {
                    self.comment.push('<');
                    self.state = CommentLessThan;
                }
                Some('-') => self.state = CommentEndDash,
                Some('\0') => self.comment.push('\u{fffd}'),
                Some(c) => self.comment.push(c),
                None => {
                    self.emit_comment();
                    self.finish();
                }
                }
            }
            CommentLessThan => match self.next() {
                Some('!') => {
                    self.comment.push('!');
                    self.state = CommentLessThanBang;
                }
                Some('<') => self.comment.push('<'),
                _ => {
                    self.reconsume();
                    self.state = Comment;
                }
            },
            CommentLessThanBang => match self.next() {
                Some('-') => self.state = CommentLessThanBangDash,
                _ => {
                    self.reconsume();
                    self.state = Comment;
                }
            },
            CommentLessThanBangDash => match self.next() {
                Some('-') => self.state = CommentLessThanBangDashDash,
                _ => {
                    self.reconsume();
                    self.state = CommentEndDash;
                }
            },
            CommentLessThanBangDashDash => {
                // '>' , EOF and anything else all reconsume in the
                // comment end state; only the error reporting differs.
                self.state = CommentEnd;
            }
            CommentEndDash => match self.next() {
                Some('-') => self.state = CommentEnd,
                None => {
                    self.emit_comment();
                    self.finish();
                }
                Some(_) => {
                    self.comment.push('-');
                    self.reconsume();
                    self.state = Comment;
                }
            },
            CommentEnd => match self.next() {
                Some('>') => {
                    self.emit_comment();
                    self.state = Data;
                }
                Some('!') => self.state = CommentEndBang,
                Some('-') => self.comment.push('-'),
                None => {
                    self.emit_comment();
                    self.finish();
                }
                Some(_) => {
                    self.comment.push_str("--");
                    self.reconsume();
                    self.state = Comment;
                }
            },
            CommentEndBang => match self.next() {
                Some('-') => {
                    self.comment.push_str("--!");
                    self.state = CommentEndDash;
                }
                Some('>') => {
                    self.emit_comment();
                    self.state = Data;
                }
                None => {
                    self.emit_comment();
                    self.finish();
                }
                Some(_) => {
                    self.comment.push_str("--!");
                    self.reconsume();
                    self.state = Comment;
                }
            },

            // --- DOCTYPE ---
            Doctype => match self.next() {
                Some(c) if is_ws(c) => self.state = BeforeDoctypeName,
                Some('>') => {
                    self.reconsume();
                    self.state = BeforeDoctypeName;
                }
                None => {
                    self.dt_name = None;
                    self.dt_force_quirks = true;
                    self.emit_doctype();
                    self.finish();
                }
                Some(_) => {
                    self.reconsume();
                    self.state = BeforeDoctypeName;
                }
            },
            BeforeDoctypeName => match self.next() {
                Some(c) if is_ws(c) => {}
                Some('\0') => {
                    self.dt_name = Some("\u{fffd}".to_string());
                    self.state = DoctypeName;
                }
                Some('>') => {
                    self.dt_force_quirks = true;
                    self.dt_name = None;
                    self.emit_doctype();
                    self.state = Data;
                }
                None => {
                    self.dt_force_quirks = true;
                    self.dt_name = None;
                    self.emit_doctype();
                    self.finish();
                }
                Some(c) => {
                    self.dt_name = Some(c.to_ascii_lowercase().to_string());
                    self.state = DoctypeName;
                }
            },
            DoctypeName => match self.next() {
                Some(c) if is_ws(c) => self.state = AfterDoctypeName,
                Some('>') => {
                    self.emit_doctype();
                    self.state = Data;
                }
                Some('\0') => {
                    self.dt_name.get_or_insert_with(String::new).push('\u{fffd}')
                }
                None => {
                    self.dt_force_quirks = true;
                    self.emit_doctype();
                    self.finish();
                }
                Some(c) => self
                    .dt_name
                    .get_or_insert_with(String::new)
                    .push(c.to_ascii_lowercase()),
            },
            AfterDoctypeName => match self.next() {
                Some(c) if is_ws(c) => {}
                Some('>') => {
                    self.emit_doctype();
                    self.state = Data;
                }
                None => {
                    self.dt_force_quirks = true;
                    self.emit_doctype();
                    self.finish();
                }
                Some(_) => {
                    self.reconsume();
                    if self.match_ahead_ci("PUBLIC") {
                        self.state = AfterDoctypePublicKeyword;
                    } else if self.match_ahead_ci("SYSTEM") {
                        self.state = AfterDoctypeSystemKeyword;
                    } else {
                        self.dt_force_quirks = true;
                        self.state = BogusDoctype;
                    }
                }
            },
            AfterDoctypePublicKeyword => match self.next() {
                Some(c) if is_ws(c) => {
                    self.state = BeforeDoctypePublicIdentifier
                }
                Some('"') => {
                    self.dt_public = Some(String::new());
                    self.state = DoctypePublicIdentifierDouble;
                }
                Some('\'') => {
                    self.dt_public = Some(String::new());
                    self.state = DoctypePublicIdentifierSingle;
                }
                Some('>') => {
                    self.dt_force_quirks = true;
                    self.emit_doctype();
                    self.state = Data;
                }
                None => {
                    self.dt_force_quirks = true;
                    self.emit_doctype();
                    self.finish();
                }
                Some(_) => {
                    self.dt_force_quirks = true;
                    self.reconsume();
                    self.state = BogusDoctype;
                }
            },
            BeforeDoctypePublicIdentifier => match self.next() {
                Some(c) if is_ws(c) => {}
                Some('"') => {
                    self.dt_public = Some(String::new());
                    self.state = DoctypePublicIdentifierDouble;
                }
                Some('\'') => {
                    self.dt_public = Some(String::new());
                    self.state = DoctypePublicIdentifierSingle;
                }
                Some('>') => {
                    self.dt_force_quirks = true;
                    self.emit_doctype();
                    self.state = Data;
                }
                None => {
                    self.dt_force_quirks = true;
                    self.emit_doctype();
                    self.finish();
                }
                Some(_) => {
                    self.dt_force_quirks = true;
                    self.reconsume();
                    self.state = BogusDoctype;
                }
            },
            DoctypePublicIdentifierDouble => match self.next() {
                Some('"') => self.state = AfterDoctypePublicIdentifier,
                Some('\0') => self
                    .dt_public
                    .get_or_insert_with(String::new)
                    .push('\u{fffd}'),
                Some('>') => {
                    self.dt_force_quirks = true;
                    self.emit_doctype();
                    self.state = Data;
                }
                None => {
                    self.dt_force_quirks = true;
                    self.emit_doctype();
                    self.finish();
                }
                Some(c) => {
                    self.dt_public.get_or_insert_with(String::new).push(c)
                }
            },
            DoctypePublicIdentifierSingle => match self.next() {
                Some('\'') => self.state = AfterDoctypePublicIdentifier,
                Some('\0') => self
                    .dt_public
                    .get_or_insert_with(String::new)
                    .push('\u{fffd}'),
                Some('>') => {
                    self.dt_force_quirks = true;
                    self.emit_doctype();
                    self.state = Data;
                }
                None => {
                    self.dt_force_quirks = true;
                    self.emit_doctype();
                    self.finish();
                }
                Some(c) => {
                    self.dt_public.get_or_insert_with(String::new).push(c)
                }
            },
            AfterDoctypePublicIdentifier => match self.next() {
                Some(c) if is_ws(c) => {
                    self.state = BetweenDoctypePublicAndSystem
                }
                Some('>') => {
                    self.emit_doctype();
                    self.state = Data;
                }
                Some('"') => {
                    self.dt_system = Some(String::new());
                    self.state = DoctypeSystemIdentifierDouble;
                }
                Some('\'') => {
                    self.dt_system = Some(String::new());
                    self.state = DoctypeSystemIdentifierSingle;
                }
                None => {
                    self.dt_force_quirks = true;
                    self.emit_doctype();
                    self.finish();
                }
                Some(_) => {
                    self.dt_force_quirks = true;
                    self.reconsume();
                    self.state = BogusDoctype;
                }
            },
            BetweenDoctypePublicAndSystem => match self.next() {
                Some(c) if is_ws(c) => {}
                Some('>') => {
                    self.emit_doctype();
                    self.state = Data;
                }
                Some('"') => {
                    self.dt_system = Some(String::new());
                    self.state = DoctypeSystemIdentifierDouble;
                }
                Some('\'') => {
                    self.dt_system = Some(String::new());
                    self.state = DoctypeSystemIdentifierSingle;
                }
                None => {
                    self.dt_force_quirks = true;
                    self.emit_doctype();
                    self.finish();
                }
                Some(_) => {
                    self.dt_force_quirks = true;
                    self.reconsume();
                    self.state = BogusDoctype;
                }
            },
            AfterDoctypeSystemKeyword => match self.next() {
                Some(c) if is_ws(c) => {
                    self.state = BeforeDoctypeSystemIdentifier
                }
                Some('"') => {
                    self.dt_system = Some(String::new());
                    self.state = DoctypeSystemIdentifierDouble;
                }
                Some('\'') => {
                    self.dt_system = Some(String::new());
                    self.state = DoctypeSystemIdentifierSingle;
                }
                Some('>') => {
                    self.dt_force_quirks = true;
                    self.emit_doctype();
                    self.state = Data;
                }
                None => {
                    self.dt_force_quirks = true;
                    self.emit_doctype();
                    self.finish();
                }
                Some(_) => {
                    self.dt_force_quirks = true;
                    self.reconsume();
                    self.state = BogusDoctype;
                }
            },
            BeforeDoctypeSystemIdentifier => match self.next() {
                Some(c) if is_ws(c) => {}
                Some('"') => {
                    self.dt_system = Some(String::new());
                    self.state = DoctypeSystemIdentifierDouble;
                }
                Some('\'') => {
                    self.dt_system = Some(String::new());
                    self.state = DoctypeSystemIdentifierSingle;
                }
                Some('>') => {
                    self.dt_force_quirks = true;
                    self.emit_doctype();
                    self.state = Data;
                }
                None => {
                    self.dt_force_quirks = true;
                    self.emit_doctype();
                    self.finish();
                }
                Some(_) => {
                    self.dt_force_quirks = true;
                    self.reconsume();
                    self.state = BogusDoctype;
                }
            },
            DoctypeSystemIdentifierDouble => match self.next() {
                Some('"') => self.state = AfterDoctypeSystemIdentifier,
                Some('\0') => self
                    .dt_system
                    .get_or_insert_with(String::new)
                    .push('\u{fffd}'),
                Some('>') => {
                    self.dt_force_quirks = true;
                    self.emit_doctype();
                    self.state = Data;
                }
                None => {
                    self.dt_force_quirks = true;
                    self.emit_doctype();
                    self.finish();
                }
                Some(c) => {
                    self.dt_system.get_or_insert_with(String::new).push(c)
                }
            },
            DoctypeSystemIdentifierSingle => match self.next() {
                Some('\'') => self.state = AfterDoctypeSystemIdentifier,
                Some('\0') => self
                    .dt_system
                    .get_or_insert_with(String::new)
                    .push('\u{fffd}'),
                Some('>') => {
                    self.dt_force_quirks = true;
                    self.emit_doctype();
                    self.state = Data;
                }
                None => {
                    self.dt_force_quirks = true;
                    self.emit_doctype();
                    self.finish();
                }
                Some(c) => {
                    self.dt_system.get_or_insert_with(String::new).push(c)
                }
            },
            AfterDoctypeSystemIdentifier => match self.next() {
                Some(c) if is_ws(c) => {}
                Some('>') => {
                    self.emit_doctype();
                    self.state = Data;
                }
                None => {
                    self.dt_force_quirks = true;
                    self.emit_doctype();
                    self.finish();
                }
                Some(_) => {
                    self.reconsume();
                    self.state = BogusDoctype;
                }
            },
            BogusDoctype => match self.next() {
                Some('>') => {
                    self.emit_doctype();
                    self.state = Data;
                }
                None => {
                    self.emit_doctype();
                    self.finish();
                }
                Some(_) => {}
            },

            // --- CDATA ---
            CdataSection => match self.next() {
                Some(']') => self.state = CdataSectionBracket,
                Some(c) => self.emit_char(c),
                None => self.finish(),
            },
            CdataSectionBracket => match self.next() {
                Some(']') => self.state = CdataSectionEnd,
                _ => {
                    self.reconsume();
                    self.emit_char(']');
                    self.state = CdataSection;
                }
            },
            CdataSectionEnd => match self.next() {
                Some(']') => self.emit_char(']'),
                Some('>') => self.state = Data,
                _ => {
                    self.reconsume();
                    self.emit_str("]]");
                    self.state = CdataSection;
                }
            },
        }
    }

    /// Shared body of the four "…EndTagName" states: they differ only
    /// in which text state they fall back to.
    fn end_tag_name_state(&mut self, fallback: State) {
        match self.next() {
            Some(c) if is_ws(c) && self.appropriate_end_tag() => {
                self.state = State::BeforeAttributeName;
            }
            Some('/') if self.appropriate_end_tag() => {
                self.state = State::SelfClosingStartTag;
            }
            Some('>') if self.appropriate_end_tag() => {
                self.emit_tag();
                self.state = State::Data;
            }
            Some(c) if c.is_ascii_alphabetic() => {
                self.tag_name.push(c.to_ascii_lowercase());
                self.temp.push(c);
            }
            _ => {
                self.reconsume();
                self.emit_str("</");
                let temp = std::mem::take(&mut self.temp);
                self.emit_str(&temp);
                self.state = fallback;
            }
        }
    }

    fn finish(&mut self) {
        self.eof = true;
        self.out.push(Token::Eof);
    }

    fn char_reference(&mut self) {
        // The spec models this as a family of states; consuming it in
        // one go here is equivalent because none of those states can
        // emit a token other than the characters this produces.
        let start = self.pos;
        let in_attr = matches!(
            self.return_state,
            State::AttributeValueDouble
                | State::AttributeValueSingle
                | State::AttributeValueUnquoted
        );
        match self.peek() {
            Some('#') => {
                self.pos += 1;
                let hex = matches!(self.peek(), Some('x') | Some('X'));
                if hex {
                    self.pos += 1;
                }
                let digits_start = self.pos;
                self.char_ref_code = 0;
                let mut overflow = false;
                while let Some(c) = self.peek() {
                    let d = if hex {
                        c.to_digit(16)
                    } else {
                        c.to_digit(10)
                    };
                    match d {
                        Some(d) => {
                            self.char_ref_code =
                                self.char_ref_code.saturating_mul(if hex {
                                    16
                                } else {
                                    10
                                });
                            self.char_ref_code =
                                self.char_ref_code.saturating_add(d);
                            if self.char_ref_code > 0x10ffff {
                                overflow = true;
                            }
                            self.pos += 1;
                        }
                        None => break,
                    }
                }
                if self.pos == digits_start {
                    // no digits: flush "&#" (and "x") as characters
                    self.pos = start;
                    self.flush_char_ref("&", in_attr);
                    self.state = self.return_state;
                    return;
                }
                if self.peek() == Some(';') {
                    self.pos += 1;
                }
                let code = if overflow { 0xfffd } else { self.char_ref_code };
                let s = numeric_replacement(code);
                self.flush_char_ref(&s, in_attr);
                self.state = self.return_state;
            }
            Some(c) if c.is_ascii_alphanumeric() => {
                // Longest match against the named table. The table is
                // sorted, and every prefix of a string sorts below it,
                // so the longest name that prefixes the input is the
                // greatest one below it: find where the input would sit
                // and walk back. Sweeping all 2231 names for every '&'
                // is otherwise the most expensive thing the tokenizer
                // does on a page with any prose in it.
                // No name is longer than 32 bytes, and all of them are
                // ASCII, so a short window is enough to decide.
                let best: Option<(usize, &'static str, bool, Option<u8>)> = {
                    let tail = &self.input[self.pos..];
                    let mut n = tail.len().min(32);
                    while n > 0 && !tail.is_char_boundary(n) {
                        n -= 1;
                    }
                    let rest = &tail[..n];
                    let at =
                        ENTITIES.partition_point(|&(nm, _)| nm <= rest);
                    let mut found = None;
                    for &(name, chars) in ENTITIES[..at].iter().rev() {
                        if rest.starts_with(name) {
                            found = Some((
                                name.len(),
                                chars,
                                name.ends_with(';'),
                                rest.as_bytes().get(name.len()).copied(),
                            ));
                            break;
                        }
                        if name.as_bytes()[0] != rest.as_bytes()[0] {
                            break;
                        }
                    }
                    found
                };
                match best {
                    Some((len, chars, semi, after)) => {
                        // Legacy rule: inside an attribute, a
                        // semicolon-less reference followed by '=' or an
                        // alphanumeric is left alone, so query strings
                        // like ?a&copy=1 survive.
                        if !semi && in_attr {
                            if after == Some(b'=')
                                || after.is_some_and(|c| c.is_ascii_alphanumeric())
                            {
                                self.flush_char_ref("&", in_attr);
                                self.state = self.return_state;
                                return;
                            }
                        }
                        self.pos += len;
                        let owned = chars.to_string();
                        self.flush_char_ref(&owned, in_attr);
                        self.state = self.return_state;
                    }
                    None => {
                        self.flush_char_ref("&", in_attr);
                        self.state = self.return_state;
                    }
                }
            }
            _ => {
                self.flush_char_ref("&", in_attr);
                self.state = self.return_state;
            }
        }
    }

    fn flush_char_ref(&mut self, s: &str, in_attr: bool) {
        if in_attr {
            self.attr_value.push_str(s);
        } else {
            self.emit_str(s);
        }
    }
}

/// The spec's numeric character reference fix-ups: the C1 range maps
/// through a legacy table, surrogates and out-of-range become U+FFFD.
fn numeric_replacement(code: u32) -> String {
    let c = match code {
        0x00 => '\u{fffd}',
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
        c if (0xd800..=0xdfff).contains(&c) => '\u{fffd}',
        c if c > 0x10ffff => '\u{fffd}',
        c => char::from_u32(c).unwrap_or('\u{fffd}'),
    };
    c.to_string()
}
