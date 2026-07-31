//! CSS parser (same grammar and error recovery as the Python parser).

use crate::dom::{bloom_slot, Document};

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
    /// :not(compound) — matches when the inner compound does not.
    Not(Vec<Simple>),
    /// :nth-child(..): 0=odd, 1=even, n>=2 -> exact index n-1 (1-based).
    NthChild(u32),
    /// :nth-of-type(..), same encoding, counting same-tag siblings only.
    NthOfType(u32),
    /// :first-of-type / :last-of-type
    FirstOfType,
    LastOfType,
    /// :first-child / :last-child
    FirstChild,
    LastChild,
    /// :hover — the element is in the document's hover chain
    Hover,
    /// :focus — the element is the document's focused node
    Focus,
}

/// How a compound relates to the compound on its right.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Combinator {
    /// `A B` — A is any ancestor of B.
    Descendant,
    /// `A > B` — A is B's parent.
    Child,
    /// `A + B` — A is the element sibling immediately before B.
    NextSibling,
    /// `A ~ B` — A is any earlier element sibling of B.
    SubsequentSibling,
}

#[derive(Clone, Debug)]
pub struct Selector {
    /// Chain of compound selectors; rightmost is last.
    pub chain: Vec<Vec<Simple>>,
    /// combinators[k] links chain[k] to chain[k+1]
    /// (always chain.len() - 1 entries).
    pub combinators: Vec<Combinator>,
    pub specificity: (u32, u32, u32),
    /// ::before / ::after — the rule styles a synthesized child of
    /// whatever the base selector matches (0 = before, 1 = after).
    pub pseudo: Option<u8>,
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
                    Simple::Not(_)
                    | Simple::NthChild(_)
                    | Simple::NthOfType(_)
                    | Simple::FirstChild
                    | Simple::LastChild
                    | Simple::FirstOfType
                    | Simple::LastOfType
                    | Simple::Hover
                    | Simple::Focus => s.1 += 1,
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
        self.matches_prefix(doc, idx, last)
    }

    /// chain[..upto] must match to the left of `idx` (which matched
    /// chain[upto]). Recursive so indefinite combinators (descendant,
    /// `~`) can backtrack past a candidate whose own left side fails.
    fn matches_prefix(&self, doc: &Document, idx: usize, upto: usize) -> bool {
        if upto == 0 {
            return true;
        }
        let comp = &self.chain[upto - 1];
        match self.combinators[upto - 1] {
            Combinator::Descendant => {
                // Most descendant candidates fail, and failing costs a
                // full walk to the root. The ancestor filter answers
                // "no" in a load and an AND.
                if !ancestor_possible(doc, idx, comp) {
                    return false;
                }
                let mut cur = doc.nodes[idx].parent;
                while let Some(p) = cur {
                    if compound_matches(doc, p, comp)
                        && self.matches_prefix(doc, p, upto - 1)
                    {
                        return true;
                    }
                    cur = doc.nodes[p].parent;
                }
                false
            }
            Combinator::Child => match doc.nodes[idx].parent {
                Some(p) => {
                    compound_matches(doc, p, comp)
                        && self.matches_prefix(doc, p, upto - 1)
                }
                None => false,
            },
            Combinator::NextSibling => {
                match prev_element_sibling(doc, idx) {
                    Some(s) => {
                        compound_matches(doc, s, comp)
                            && self.matches_prefix(doc, s, upto - 1)
                    }
                    None => false,
                }
            }
            Combinator::SubsequentSibling => {
                let mut cur = prev_element_sibling(doc, idx);
                while let Some(s) = cur {
                    if compound_matches(doc, s, comp)
                        && self.matches_prefix(doc, s, upto - 1)
                    {
                        return true;
                    }
                    cur = prev_element_sibling(doc, s);
                }
                false
            }
        }
    }
}

/// The most selective name in `compound` (id > class > tag) as an
/// ancestor-bloom slot. `None` when nothing in it is hashable (`*`,
/// `:hover`, attribute-only, `:not(...)` — whose inner names are
/// negated and must never be looked up), and then the filter cannot
/// reject.
fn compound_bloom_slot(compound: &[Simple]) -> Option<(usize, u64)> {
    let mut best: Option<(u8, &str)> = None;
    for part in compound {
        let cand = match part {
            Simple::Id(v) => (3u8, v.as_str()),
            Simple::Class(v) => (2, v.as_str()),
            Simple::Tag(v) => (1, v.as_str()),
            _ => continue,
        };
        if best.is_none_or(|(rank, _)| cand.0 > rank) {
            best = Some(cand);
        }
    }
    best.map(|(_, name)| bloom_slot(name))
}

/// Could any ancestor of `idx` match `compound`? `false` is definitive;
/// `true` still needs the walk. Answers `true` whenever the filter is
/// missing or stale, so a wrong answer is never possible — only a
/// slower one.
fn ancestor_possible(
    doc: &Document,
    idx: usize,
    compound: &[Simple],
) -> bool {
    if doc.ancestor_bloom_version != doc.version {
        return true;
    }
    let Some(filter) = doc.ancestor_bloom.get(idx) else {
        return true;
    };
    match compound_bloom_slot(compound) {
        Some((word, bit)) => filter[word] & bit != 0,
        None => true,
    }
}

/// The nearest element sibling before `idx` (None at the front).
fn prev_element_sibling(doc: &Document, idx: usize) -> Option<usize> {
    let p = doc.nodes[idx].parent?;
    let mut prev = None;
    for &c in &doc.nodes[p].children {
        if c == idx {
            return prev;
        }
        if doc.nodes[c].is_element() {
            prev = Some(c);
        }
    }
    None
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
        Simple::Hover => doc.hover_chain.contains(&idx),
        Simple::Focus => doc.focused == Some(idx),
        Simple::Where(options) => options
            .iter()
            .any(|opt| compound_matches(doc, idx, opt)),
        Simple::Not(inner) => !compound_matches(doc, idx, inner),
        Simple::NthChild(spec) => {
            let Some(p) = node.parent else { return false };
            let sibs: Vec<usize> = doc.nodes[p]
                .children
                .iter()
                .copied()
                .filter(|&c| doc.nodes[c].is_element())
                .collect();
            let Some(pos) = sibs.iter().position(|&c| c == idx) else {
                return false;
            };
            let nth = pos + 1; // 1-based
            match spec {
                0 => nth % 2 == 1, // odd
                1 => nth % 2 == 0, // even
                k => nth == (*k as usize) - 1,
            }
        }
        Simple::NthOfType(spec) => {
            let Some(p) = node.parent else { return false };
            let sibs: Vec<usize> = doc.nodes[p]
                .children
                .iter()
                .copied()
                .filter(|&c| {
                    doc.nodes[c].is_element()
                        && doc.nodes[c].tag == node.tag
                })
                .collect();
            let Some(pos) = sibs.iter().position(|&c| c == idx) else {
                return false;
            };
            let nth = pos + 1;
            match spec {
                0 => nth % 2 == 1,
                1 => nth % 2 == 0,
                k => nth == (*k as usize) - 1,
            }
        }
        Simple::FirstOfType | Simple::LastOfType => {
            let Some(p) = node.parent else { return false };
            let sibs: Vec<usize> = doc.nodes[p]
                .children
                .iter()
                .copied()
                .filter(|&c| {
                    doc.nodes[c].is_element()
                        && doc.nodes[c].tag == node.tag
                })
                .collect();
            match part {
                Simple::FirstOfType => sibs.first() == Some(&idx),
                _ => sibs.last() == Some(&idx),
            }
        }
        Simple::FirstChild | Simple::LastChild => {
            let Some(p) = node.parent else { return false };
            let sibs: Vec<usize> = doc.nodes[p]
                .children
                .iter()
                .copied()
                .filter(|&c| doc.nodes[c].is_element())
                .collect();
            match part {
                Simple::FirstChild => sibs.first() == Some(&idx),
                _ => sibs.last() == Some(&idx),
            }
        }
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

/// A media-feature length in px (em taken as 16px).
fn media_len(v: &str) -> Option<f64> {
    let v = v.trim();
    if let Some(n) = v.strip_suffix("px") {
        n.trim().parse().ok()
    } else if let Some(n) = v.strip_suffix("em") {
        n.trim().parse::<f64>().ok().map(|x| x * 16.0)
    } else if let Some(n) = v.strip_suffix("rem") {
        n.trim().parse::<f64>().ok().map(|x| x * 16.0)
    } else {
        v.parse().ok()
    }
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
    /// viewport width in px, for evaluating @media (min/max-width: ...)
    vw: f64,
}

impl<'a> CssParser<'a> {
    pub fn new(s: &'a str) -> Self {
        CssParser {
            s,
            b: s.as_bytes(),
            i: 0,
            vw: 1280.0, // desktop default; set_viewport overrides
        }
    }

    pub fn set_viewport(&mut self, vw: f64) {
        self.vw = vw;
    }

/// Markup that is legal around a stylesheet's contents but is not CSS.
///
/// `<!--` and `-->` are the CDO/CDC tokens CSS has always told parsers
/// to skip at the top level (they let a 1996 stylesheet hide from
/// browsers that would have shown it as text). `<![CDATA[` and `]]>`
/// are the XHTML spelling of the same idea, and the CSS corpus is full
/// of `.xht` references that wrap their whole stylesheet in one --
/// 2437 test files point at those references, and every one of them
/// was rendering unstyled because the rule after the delimiter never
/// parsed.
fn leading_noise(rest: &str) -> Option<usize> {
    for token in ["<![CDATA[", "]]>", "<!--", "-->"] {
        if rest.starts_with(token) {
            return Some(token.len());
        }
    }
    None
}

    fn whitespace(&mut self) {
        loop {
            while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace()
            {
                self.i += 1;
            }
            let rest = &self.s[self.i.min(self.s.len())..];
            if rest.starts_with("/*") {
                match self.s[self.i + 2..].find("*/") {
                    Some(e) => self.i = self.i + 2 + e + 2,
                    None => self.i = self.b.len(),
                }
            } else if let Some(skip) = Self::leading_noise(rest) {
                self.i += skip;
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
                } else if self.peek_ci(":not(") {
                    self.i += 5;
                    let inner = self.simple_selector()?;
                    self.whitespace();
                    self.literal(b')')?;
                    parts.push(Simple::Not(inner));
                } else if self.peek_ci(":nth-child(") {
                    self.i += 11;
                    let arg = self.until_chars(b")").trim()
                        .to_ascii_lowercase();
                    self.literal(b')')?;
                    let spec = match arg.as_str() {
                        "odd" => 0,
                        "even" => 1,
                        n => match n.parse::<u32>() {
                            Ok(k) => k + 1,
                            Err(_) => return Err(()), // an+b: reject rule
                        },
                    };
                    parts.push(Simple::NthChild(spec));
                } else if self.peek_ci(":nth-of-type(") {
                    self.i += 13;
                    let arg = self.until_chars(b")").trim()
                        .to_ascii_lowercase();
                    self.literal(b')')?;
                    let spec = match arg.as_str() {
                        "odd" => 0,
                        "even" => 1,
                        n => match n.parse::<u32>() {
                            Ok(k) => k + 1,
                            Err(_) => return Err(()),
                        },
                    };
                    parts.push(Simple::NthOfType(spec));
                } else if self.peek_ci(":first-of-type")
                    && !self.name_char_at(14)
                {
                    self.i += 14;
                    parts.push(Simple::FirstOfType);
                } else if self.peek_ci(":last-of-type")
                    && !self.name_char_at(13)
                {
                    self.i += 13;
                    parts.push(Simple::LastOfType);
                } else if self.peek_ci(":first-child")
                    && !self.name_char_at(12)
                {
                    self.i += 12;
                    parts.push(Simple::FirstChild);
                } else if self.peek_ci(":last-child")
                    && !self.name_char_at(11)
                {
                    self.i += 11;
                    parts.push(Simple::LastChild);
                } else if self.peek_ci(":hover")
                    && !self.name_char_at(6)
                {
                    self.i += 6;
                    parts.push(Simple::Hover);
                } else if self.peek_ci(":focus")
                    && !self.name_char_at(6)
                {
                    self.i += 6;
                    parts.push(Simple::Focus);
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
        let mut combinators = Vec::new();
        self.whitespace();
        while self.i < self.b.len()
            && self.b[self.i] != b'{'
            && self.b[self.i] != b','
        {
            let mut comb = Combinator::Descendant;
            if b">+~".contains(&self.b[self.i]) {
                comb = match self.b[self.i] {
                    b'>' => Combinator::Child,
                    b'+' => Combinator::NextSibling,
                    _ => Combinator::SubsequentSibling,
                };
                self.i += 1;
                self.whitespace();
            }
            // ::before/::after terminate the selector as a pseudo
            // element; other pseudo suffixes (:hover) reject the rule
            if self.i < self.b.len() && self.b[self.i] == b':' {
                if let Some(p) = self.eat_pseudo_element() {
                    let specificity =
                        Selector::compute_specificity(&chain);
                    return Ok(Selector {
                        chain,
                        combinators,
                        specificity,
                        pseudo: Some(p),
                    });
                }
                if !self.peek_ci(":root")
                    && !self.peek_ci(":where(")
                    && !self.peek_ci(":not(")
                    && !self.peek_ci(":nth-child(")
                    && !self.peek_ci(":first-child")
                    && !self.peek_ci(":last-child")
                    && !self.peek_ci(":hover")
                    && !self.peek_ci(":focus")
                {
                    return Err(());
                }
            }
            chain.push(self.simple_selector()?);
            combinators.push(comb);
            self.whitespace();
        }
        let specificity = Selector::compute_specificity(&chain);
        Ok(Selector { chain, combinators, specificity, pseudo: None })
    }

    /// Consume ::before/::after (and the legacy single-colon forms).
    fn eat_pseudo_element(&mut self) -> Option<u8> {
        for (pat, p) in [("::before", 0u8), ("::after", 1),
                         (":before", 0), (":after", 1)] {
            if self.peek_ci(pat) {
                self.i += pat.len();
                return Some(p);
            }
        }
        None
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
            // @media whose condition matches the viewport is unwrapped
            // and its inner rules parsed at this level; everything else
            // (and non-matching media) is skipped.
            if self.s[self.i..].to_ascii_lowercase().starts_with("@media") {
                self.parse_media(rules);
            } else {
                self.skip_at_rule();
            }
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

    /// Parse `@media <condition> { <rules> }`. When the condition
    /// matches the viewport, the inner rules are parsed into `rules`;
    /// otherwise the whole block is skipped. `self.i` is at '@'.
    fn parse_media(&mut self, rules: &mut Vec<Rule>) {
        self.i += 6; // past "@media"
        let cond_start = self.i;
        while self.i < self.b.len() && self.b[self.i] != b'{' {
            self.i += 1;
        }
        let cond = self.s[cond_start..self.i].to_ascii_lowercase();
        if self.i >= self.b.len() {
            return;
        }
        self.i += 1; // past '{'
        if self.media_matches(&cond) {
            // parse inner rules until the matching '}'
            loop {
                self.whitespace();
                if self.i >= self.b.len() || self.b[self.i] == b'}' {
                    break;
                }
                match self.parse_one(rules) {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(()) => match self.ignore_until(b"};") {
                        Some(b'}') => {
                            // could be the media block's close; peek
                            self.i += 1;
                        }
                        Some(_) => self.i += 1,
                        None => break,
                    },
                }
            }
            if self.i < self.b.len() && self.b[self.i] == b'}' {
                self.i += 1;
            }
        } else {
            // skip the block body (depth 1 already entered)
            let mut depth = 1usize;
            while self.i < self.b.len() && depth > 0 {
                match self.b[self.i] {
                    b'{' => depth += 1,
                    b'}' => depth -= 1,
                    _ => {}
                }
                self.i += 1;
            }
        }
    }

    /// Evaluate a media condition against the viewport. Supports
    /// min-width/max-width in px/em, the `screen`/`all` types, and
    /// `and`. Unknown features are treated as matching (so a rule is
    /// never lost to a feature we do not model). Comma = any (or).
    fn media_matches(&self, cond: &str) -> bool {
        cond.split(',').any(|q| self.media_query_matches(q.trim()))
    }

    fn media_query_matches(&self, q: &str) -> bool {
        // normalize whitespace first: a query formatted across lines
        // ("screen\nand (max-width:750px)") must still split on `and`,
        // otherwise the feature is never tested and the query wrongly
        // matches — leaking mobile rules onto desktop.
        let norm = q.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut ok = true;
        for part in norm.split(" and ") {
            let p = part.trim().trim_start_matches("only ").trim();
            if p.is_empty() || p == "screen" || p == "all" {
                continue;
            }
            if p == "print" || p == "speech" {
                return false;
            }
            if let Some(inner) =
                p.strip_prefix('(').and_then(|x| x.strip_suffix(')'))
            {
                if let Some((feat, val)) = inner.split_once(':') {
                    let feat = feat.trim();
                    let px = media_len(val.trim());
                    match (feat, px) {
                        ("min-width", Some(v)) => ok &= self.vw >= v,
                        ("max-width", Some(v)) => ok &= self.vw <= v,
                        // width/height/orientation/etc: don't reject
                        _ => {}
                    }
                }
            }
        }
        ok
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
