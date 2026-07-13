//! CSS parser (same grammar and error recovery as the Python parser).

use crate::dom::Document;

#[derive(Clone, Debug)]
pub enum Simple {
    Tag(String),
    Class(String),
    Id(String),
    Universal,
    /// :root — the document root element (pseudo-class specificity).
    Root,
    /// :where(a, b, ...) — any alternative matches; zero specificity.
    /// Unsupported alternatives are dropped at parse time.
    Where(Vec<Vec<Simple>>),
    /// [attr] / [attr<op>=value] — op is 0 (presence) or one of
    /// b'=' b'~' b'^' b'$' b'*' b'|' (class-level specificity).
    Attr { name: String, op: u8, value: String },
}

#[derive(Clone, Debug)]
pub struct Selector {
    /// Descendant chain of compound selectors; rightmost is last.
    pub chain: Vec<Vec<Simple>>,
    pub specificity: (u32, u32, u32),
}

impl Selector {
    fn compute_specificity(chain: &[Vec<Simple>]) -> (u32, u32, u32) {
        let mut s = (0, 0, 0);
        for compound in chain {
            for part in compound {
                match part {
                    Simple::Id(_) => s.0 += 1,
                    Simple::Class(_) => s.1 += 1,
                    Simple::Tag(_) => s.2 += 1,
                    Simple::Root => s.1 += 1,
                    Simple::Attr { .. } => s.1 += 1,
                    Simple::Universal | Simple::Where(_) => {}
                }
            }
        }
        s
    }

    pub fn matches(&self, doc: &Document, idx: usize) -> bool {
        let last = self.chain.len() - 1;
        if !compound_matches(doc, idx, &self.chain[last]) {
            return false;
        }
        let mut cur = doc.nodes[idx].parent;
        for comp in self.chain[..last].iter().rev() {
            loop {
                match cur {
                    None => return false,
                    Some(p) => {
                        let hit = compound_matches(doc, p, comp);
                        cur = doc.nodes[p].parent;
                        if hit {
                            break;
                        }
                    }
                }
            }
        }
        true
    }
}

pub fn compound_matches(doc: &Document, idx: usize, compound: &[Simple]) -> bool {
    let node = &doc.nodes[idx];
    if !node.is_element() {
        return false;
    }
    compound.iter().all(|part| match part {
        Simple::Tag(t) => node.tag.as_deref() == Some(t.as_str()),
        Simple::Class(c) => node.classes.iter().any(|x| x == c),
        Simple::Id(i) => node.attr("id") == Some(i.as_str()),
        Simple::Universal => true,
        Simple::Root => node.tag.as_deref() == Some("html"),
        Simple::Where(options) => options
            .iter()
            .any(|opt| compound_matches(doc, idx, opt)),
        Simple::Attr { name, op, value } => match node.attr(name) {
            None => false,
            Some(actual) => match op {
                0 => true,
                b'=' => actual == value,
                b'~' => actual.split_whitespace().any(|w| w == value),
                b'^' => !value.is_empty() && actual.starts_with(value),
                b'$' => !value.is_empty() && actual.ends_with(value),
                b'*' => !value.is_empty() && actual.contains(value),
                b'|' => {
                    actual == value
                        || actual.starts_with(&format!("{value}-"))
                }
                _ => false,
            },
        },
    })
}

pub struct Rule {
    pub selector: Selector,
    pub decls: Vec<(String, String)>,
    /// Cascade origin: 0 = user-agent sheet, 1 = author. Origin
    /// outranks specificity (set by style::compute_styles).
    pub origin: u8,
}

/// Parse a selector list like "div.a, #b span" (for querySelector).
pub fn parse_selector_list(s: &str) -> Vec<Selector> {
    let mut p = CssParser::new(s);
    let mut out = Vec::new();
    loop {
        p.whitespace();
        match p.selector() {
            Ok(sel) => out.push(sel),
            Err(()) => break,
        }
        p.whitespace();
        if p.literal(b',').is_err() {
            break;
        }
    }
    out
}

pub struct CssParser<'a> {
    s: &'a str,
    b: &'a [u8],
    i: usize,
}

impl<'a> CssParser<'a> {
    pub fn new(s: &'a str) -> Self {
        CssParser {
            s,
            b: s.as_bytes(),
            i: 0,
        }
    }

    fn whitespace(&mut self) {
        loop {
            while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace()
            {
                self.i += 1;
            }
            if self.s[self.i.min(self.s.len())..].starts_with("/*") {
                match self.s[self.i + 2..].find("*/") {
                    Some(e) => self.i = self.i + 2 + e + 2,
                    None => self.i = self.b.len(),
                }
            } else {
                break;
            }
        }
    }

    fn word(&mut self) -> Result<&'a str, ()> {
        let start = self.i;
        while self.i < self.b.len() {
            let c = self.b[self.i];
            if c.is_ascii_alphanumeric()
                || b"#-_.%!\"'(),".contains(&c)
                || c >= 0x80
            {
                self.i += 1;
            } else {
                break;
            }
        }
        if self.i <= start {
            return Err(());
        }
        Ok(&self.s[start..self.i])
    }

    fn literal(&mut self, ch: u8) -> Result<(), ()> {
        if self.i >= self.b.len() || self.b[self.i] != ch {
            return Err(());
        }
        self.i += 1;
        Ok(())
    }

    fn until_chars(&mut self, chars: &[u8]) -> &'a str {
        let start = self.i;
        while self.i < self.b.len() && !chars.contains(&self.b[self.i]) {
            self.i += 1;
        }
        &self.s[start..self.i]
    }

    fn pair(&mut self) -> Result<(String, String), ()> {
        let prop = self.word()?.to_ascii_lowercase();
        self.whitespace();
        self.literal(b':')?;
        self.whitespace();
        let mut value = self.until_chars(b";}").trim().to_string();
        // Strip !important so it can't poison the value (parity with
        // the Python parser; priority approximated by source order).
        let low = value.to_ascii_lowercase();
        if low.ends_with("important") {
            let head = value[..value.len() - "important".len()].trim_end();
            if let Some(stripped) = head.strip_suffix('!') {
                value = stripped.trim_end().to_string();
            }
        }
        Ok((prop, value))
    }

    fn ignore_until(&mut self, chars: &[u8]) -> Option<u8> {
        while self.i < self.b.len() {
            if chars.contains(&self.b[self.i]) {
                return Some(self.b[self.i]);
            }
            self.i += 1;
        }
        None
    }

    pub fn body(&mut self) -> Vec<(String, String)> {
        let mut pairs = Vec::new();
        self.whitespace(); // tolerate a leading-space style attribute
        while self.i < self.b.len() && self.b[self.i] != b'}' {
            match self.pair() {
                Ok(pv) => {
                    pairs.push(pv);
                    self.whitespace();
                    if self.literal(b';').is_err() {
                        break;
                    }
                    self.whitespace();
                }
                Err(()) => match self.ignore_until(b";}") {
                    Some(b';') => {
                        self.i += 1;
                        self.whitespace();
                    }
                    _ => break,
                },
            }
        }
        pairs
    }

    fn name(&mut self) -> Result<&'a str, ()> {
        let start = self.i;
        while self.i < self.b.len() {
            let c = self.b[self.i];
            if c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c >= 0x80
            {
                self.i += 1;
            } else {
                break;
            }
        }
        if self.i <= start {
            return Err(());
        }
        Ok(&self.s[start..self.i])
    }

    fn simple_selector(&mut self) -> Result<Vec<Simple>, ()> {
        let mut parts = Vec::new();
        while self.i < self.b.len() {
            let c = self.b[self.i];
            if c == b'*' {
                self.i += 1;
                parts.push(Simple::Universal);
            } else if c == b'.' {
                self.i += 1;
                parts.push(Simple::Class(self.name()?.to_string()));
            } else if c == b'#' {
                self.i += 1;
                parts.push(Simple::Id(self.name()?.to_string()));
            } else if c.is_ascii_alphanumeric() || c == b'-' || c == b'_' {
                parts.push(Simple::Tag(self.name()?.to_ascii_lowercase()));
            } else if c == b'[' {
                self.i += 1;
                parts.push(self.attribute()?);
            } else if c == b':' {
                if self.peek_ci(":where(") {
                    self.i += 7;
                    parts.push(Simple::Where(self.where_options()));
                } else if self.peek_ci(":root") && !self.name_char_at(5) {
                    self.i += 5;
                    parts.push(Simple::Root);
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        if parts.is_empty() {
            return Err(());
        }
        Ok(parts)
    }

    /// Case-insensitive lookahead at the current position.
    fn peek_ci(&self, what: &str) -> bool {
        let end = self.i + what.len();
        end <= self.b.len()
            && self.b[self.i..end].eq_ignore_ascii_case(what.as_bytes())
    }

    /// Is the byte at self.i + offset an identifier character?
    fn name_char_at(&self, offset: usize) -> bool {
        match self.b.get(self.i + offset) {
            Some(&c) => {
                c.is_ascii_alphanumeric() || c == b'-' || c == b'_'
                    || c >= 0x80
            }
            None => false,
        }
    }

    /// Parse an [attr...] selector ('[' already consumed).
    fn attribute(&mut self) -> Result<Simple, ()> {
        self.whitespace();
        let name = self.name()?.to_ascii_lowercase();
        self.whitespace();
        if self.literal(b']').is_ok() {
            return Ok(Simple::Attr { name, op: 0, value: String::new() });
        }
        let mut op = b'=';
        if self.i < self.b.len() && b"~^$*|".contains(&self.b[self.i]) {
            op = self.b[self.i];
            self.i += 1;
        }
        self.literal(b'=')?;
        self.whitespace();
        let value;
        if self.i < self.b.len()
            && (self.b[self.i] == b'\'' || self.b[self.i] == b'"')
        {
            let quote = self.b[self.i];
            self.i += 1;
            let start = self.i;
            while self.i < self.b.len() && self.b[self.i] != quote {
                self.i += 1;
            }
            value = self.s[start..self.i].to_string();
            self.i += 1;
        } else {
            let start = self.i;
            while self.i < self.b.len()
                && self.b[self.i] != b']'
                && !self.b[self.i].is_ascii_whitespace()
            {
                self.i += 1;
            }
            value = self.s[start..self.i].to_string();
        }
        self.whitespace();
        // tolerate (and ignore) the case-sensitivity flag "[a=b i]"
        if self.i < self.b.len()
            && matches!(self.b[self.i], b'i' | b'I' | b's' | b'S')
        {
            self.i += 1;
            self.whitespace();
        }
        self.literal(b']')?;
        Ok(Simple::Attr { name, op, value })
    }

    /// Parse :where() alternatives (self.i is just past ":where(").
    /// Each alternative must be a compound selector we support and end
    /// cleanly at ',' or ')' — otherwise it is dropped, not the rule.
    fn where_options(&mut self) -> Vec<Vec<Simple>> {
        let mut options = Vec::new();
        while self.i < self.b.len() {
            self.whitespace();
            if self.i < self.b.len() && self.b[self.i] == b')' {
                self.i += 1;
                break;
            }
            let parsed = self.simple_selector();
            self.whitespace();
            let clean = self.i < self.b.len()
                && (self.b[self.i] == b',' || self.b[self.i] == b')');
            if let Ok(compound) = parsed {
                if clean {
                    options.push(compound);
                }
            }
            let mut depth = 0usize; // skip an unsupported alternative
            while self.i < self.b.len() {
                match self.b[self.i] {
                    b'(' => depth += 1,
                    b')' => {
                        if depth == 0 {
                            break;
                        }
                        depth -= 1;
                    }
                    b',' if depth == 0 => break,
                    _ => {}
                }
                self.i += 1;
            }
            if self.i < self.b.len() && self.b[self.i] == b',' {
                self.i += 1;
                continue;
            }
            if self.i < self.b.len() && self.b[self.i] == b')' {
                self.i += 1;
            }
            break;
        }
        options
    }

    fn selector(&mut self) -> Result<Selector, ()> {
        let mut chain = vec![self.simple_selector()?];
        self.whitespace();
        while self.i < self.b.len()
            && self.b[self.i] != b'{'
            && self.b[self.i] != b','
        {
            // >, +, ~ degrade to descendant (same as Python engine)
            if b">+~".contains(&self.b[self.i]) {
                self.i += 1;
                self.whitespace();
            }
            // pseudo suffixes like :hover are unsupported -> reject rule
            // (:root / :where( / [attr] are handled by simple_selector)
            if self.i < self.b.len()
                && self.b[self.i] == b':'
                && !self.peek_ci(":root")
                && !self.peek_ci(":where(")
            {
                return Err(());
            }
            chain.push(self.simple_selector()?);
            self.whitespace();
        }
        let specificity = Selector::compute_specificity(&chain);
        Ok(Selector { chain, specificity })
    }

    pub fn parse(&mut self) -> Vec<Rule> {
        let mut rules = Vec::new();
        while self.i < self.b.len() {
            match self.parse_one(&mut rules) {
                Ok(true) => {}
                Ok(false) => break,
                Err(()) => match self.ignore_until(b"};") {
                    Some(_) => self.i += 1,
                    None => break,
                },
            }
        }
        rules
    }

    fn parse_one(&mut self, rules: &mut Vec<Rule>) -> Result<bool, ()> {
        self.whitespace();
        if self.i >= self.b.len() {
            return Ok(false);
        }
        if self.b[self.i] == b'@' {
            self.skip_at_rule();
            return Ok(true);
        }
        let mut selectors = vec![self.selector()?];
        self.whitespace();
        while self.i < self.b.len() && self.b[self.i] == b',' {
            self.literal(b',')?;
            self.whitespace();
            selectors.push(self.selector()?);
            self.whitespace();
        }
        self.literal(b'{')?;
        self.whitespace();
        let body = self.body();
        self.literal(b'}')?;
        for sel in selectors {
            rules.push(Rule {
                selector: sel,
                decls: body.clone(),
                origin: 1,
            });
        }
        Ok(true)
    }

    fn skip_at_rule(&mut self) {
        while self.i < self.b.len()
            && self.b[self.i] != b';'
            && self.b[self.i] != b'{'
        {
            self.i += 1;
        }
        if self.i >= self.b.len() {
            return;
        }
        if self.b[self.i] == b';' {
            self.i += 1;
            return;
        }
        let mut depth = 0usize;
        while self.i < self.b.len() {
            match self.b[self.i] {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        self.i += 1;
                        return;
                    }
                }
                _ => {}
            }
            self.i += 1;
        }
    }
}
