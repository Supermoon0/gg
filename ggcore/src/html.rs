//! HTML tokenizer + tree builder (same semantics as the Python parser).

use crate::dom::Document;

const SELF_CLOSING: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link",
    "meta", "param", "source", "track", "wbr",
];

const HEAD_TAGS: &[&str] = &[
    "base", "basefont", "bgsound", "noscript", "link", "meta", "title",
    "style", "script",
];

const RAW_TEXT: &[&str] = &["script", "style"];

const P_CLOSERS: &[&str] = &[
    "p", "div", "h1", "h2", "h3", "h4", "h5", "h6", "ul", "ol", "li",
    "table", "blockquote", "pre", "form", "hr", "section", "article",
    "header", "footer", "nav", "aside", "main", "figure",
];

pub struct Parser<'a> {
    body: &'a str,
    bytes: &'a [u8],
    doc: Document,
    unfinished: Vec<usize>,
}

pub fn parse(body: &str) -> Document {
    let mut p = Parser {
        body,
        bytes: body.as_bytes(),
        doc: Document::with_capacity(1024),
        unfinished: Vec::new(),
    };
    p.run();
    p.doc
}

impl<'a> Parser<'a> {
    fn run(&mut self) {
        let n = self.bytes.len();
        let mut i = 0;
        let mut text_start = i;
        let mut text = String::new();

        while i < n {
            if self.bytes[i] == b'<' {
                if self.body[i..].starts_with("<!--") {
                    text.push_str(&self.body[text_start..i]);
                    i = match find_sub(self.body, "-->", i + 4) {
                        Some(e) => e + 3,
                        None => n,
                    };
                    text_start = i;
                    continue;
                }
                if self.body[i..].starts_with("<!") {
                    text.push_str(&self.body[text_start..i]);
                    i = match memchr(self.bytes, b'>', i) {
                        Some(e) => e + 1,
                        None => n,
                    };
                    text_start = i;
                    continue;
                }
                let end = match self.find_tag_end(i) {
                    Some(e) => e,
                    None => {
                        text.push_str(&self.body[text_start..]);
                        text_start = n;
                        break;
                    }
                };
                text.push_str(&self.body[text_start..i]);
                if !text.is_empty() {
                    self.add_text(std::mem::take(&mut text), false);
                }
                let tag_content = &self.body[i + 1..end];
                i = end + 1;
                text_start = i;
                let tag_name = self.add_tag(tag_content);

                if RAW_TEXT.contains(&tag_name.as_str()) {
                    let close_pat = format!("</{}", tag_name);
                    let close = find_ci(self.body, &close_pat, i).unwrap_or(n);
                    let raw = &self.body[i..close];
                    if !raw.is_empty() {
                        self.add_text(raw.to_string(), true);
                    }
                    i = match memchr(self.bytes, b'>', close) {
                        Some(g) => g + 1,
                        None => n,
                    };
                    text_start = i;
                    self.add_tag(&format!("/{}", tag_name));
                }
            } else {
                i = memchr(self.bytes, b'<', i).unwrap_or(n);
            }
        }
        text.push_str(&self.body[text_start..n.min(self.body.len())]);
        if !text.is_empty() {
            self.add_text(text, false);
        }
        self.finish();
    }

    fn find_tag_end(&self, start: usize) -> Option<usize> {
        let mut i = start + 1;
        let n = self.bytes.len();
        let mut quote: Option<u8> = None;
        while i < n {
            let c = self.bytes[i];
            match quote {
                Some(q) => {
                    if c == q {
                        quote = None;
                    }
                }
                None => {
                    if c == b'\'' || c == b'"' {
                        quote = Some(c);
                    } else if c == b'>' {
                        return Some(i);
                    }
                }
            }
            i += 1;
        }
        None
    }

    fn add_text(&mut self, text: String, raw: bool) {
        if text.trim().is_empty() {
            return;
        }
        self.implicit_tags(None);
        let text = if raw { text } else { unescape(&text) };
        let parent = *self.unfinished.last().unwrap();
        self.doc.new_text(text, parent);
    }

    fn add_tag(&mut self, tag_content: &str) -> String {
        let (tag, attrs) = get_attributes(tag_content);
        if tag.is_empty() || tag.starts_with('?') {
            return tag;
        }
        self.implicit_tags(Some(&tag));
        if let Some(name) = tag.strip_prefix('/') {
            self.close_tag(name);
        } else if (SELF_CLOSING.contains(&tag.as_str())
            || tag_content.trim_end().ends_with('/'))
            && !self.unfinished.is_empty()
        {
            // (empty stack = a self-closed root like <html/> — fall
            // through and open it normally, as real browsers do)
            let parent = *self.unfinished.last().unwrap();
            self.doc.new_element(tag.clone(), attrs, Some(parent));
        } else {
            if let Some(&top) = self.unfinished.last() {
                let open = self.doc.nodes[top].tag.as_deref().unwrap_or("");
                if open == "p" && P_CLOSERS.contains(&tag.as_str()) {
                    self.close_tag("p");
                } else if open == "li" && tag == "li" {
                    self.close_tag("li");
                } else if open == "option"
                    && (tag == "option" || tag == "optgroup")
                {
                    // without this `<option>a<option>b` nests, and a
                    // <select>'s painted label swallows every later
                    // option's text (mirrors browser/html_parser.py)
                    self.close_tag("option");
                } else if open == "optgroup" && tag == "optgroup" {
                    self.close_tag("optgroup");
                }
            }
            let parent = self.unfinished.last().copied();
            let idx = self.doc.new_element(tag.clone(), attrs, parent);
            self.unfinished.push(idx);
        }
        tag
    }

    fn close_tag(&mut self, tag: &str) {
        if self.unfinished.len() <= 1 {
            return;
        }
        for idx in (1..self.unfinished.len()).rev() {
            let node = self.unfinished[idx];
            if self.doc.nodes[node].tag.as_deref() == Some(tag) {
                self.unfinished.truncate(idx);
                return;
            }
        }
    }

    fn open_tags(&self) -> Vec<&str> {
        self.unfinished
            .iter()
            .map(|&i| self.doc.nodes[i].tag.as_deref().unwrap_or(""))
            .collect()
    }

    fn implicit_tags(&mut self, tag: Option<&str>) {
        loop {
            let open = self.open_tags();
            if open.is_empty() && tag != Some("html") {
                self.add_tag("html");
            } else if open == ["html"]
                && !matches!(tag, Some("head") | Some("body") | Some("/html"))
            {
                if let Some(t) = tag {
                    if HEAD_TAGS.contains(&t) {
                        self.add_tag("head");
                        continue;
                    }
                }
                self.add_tag("body");
            } else if open == ["html", "head"] {
                let in_head = match tag {
                    Some("/head") => true,
                    Some(t) => HEAD_TAGS.contains(&t),
                    None => false,
                };
                if !in_head {
                    self.add_tag("/head");
                } else {
                    break;
                }
            } else {
                break;
            }
        }
    }

    fn finish(&mut self) {
        if self.unfinished.is_empty() {
            self.implicit_tags(None);
        }
        self.doc.root = self.unfinished[0];
        self.unfinished.clear();
    }
}

fn get_attributes(text: &str) -> (String, Vec<(String, String)>) {
    let bytes = text.as_bytes();
    let n = bytes.len();
    let mut i = 0;
    // Tag name: ends at whitespace or the self-closing slash (a
    // leading '/' belongs to a close tag's name).
    if i < n && bytes[i] == b'/' {
        i += 1;
    }
    while i < n && !bytes[i].is_ascii_whitespace() && bytes[i] != b'/' {
        i += 1;
    }
    let tag = text[..i].to_ascii_lowercase();
    let mut attrs: Vec<(String, String)> = Vec::new();

    while i < n {
        while i < n && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }
        if bytes[i] == b'/' {
            i += 1; // stray solidus in a tag is ignored (spec)
            continue;
        }
        let start = i;
        while i < n
            && bytes[i] != b'='
            && bytes[i] != b'/'
            && !bytes[i].is_ascii_whitespace()
        {
            i += 1;
        }
        let name = text[start..i].to_ascii_lowercase();
        if name.is_empty() {
            i += 1; // stray '=': skip it, keep later attributes
            continue;
        }
        while i < n && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < n && bytes[i] == b'=' {
            i += 1;
            while i < n && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < n && (bytes[i] == b'\'' || bytes[i] == b'"') {
                let quote = bytes[i];
                i += 1;
                let vstart = i;
                while i < n && bytes[i] != quote {
                    i += 1;
                }
                value = text[vstart..i].to_string();
                i += 1;
            } else {
                let vstart = i;
                while i < n && !bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                value = text[vstart..i].to_string();
            }
        }
        if !attrs.iter().any(|(k, _)| *k == name) {
            // spec: first duplicate wins (keeps Python parity too)
            attrs.push((name, unescape(&value)));
        }
    }
    (tag, attrs)
}

// ----- helpers -----

pub fn memchr(haystack: &[u8], needle: u8, from: usize) -> Option<usize> {
    haystack[from.min(haystack.len())..]
        .iter()
        .position(|&b| b == needle)
        .map(|p| p + from)
}

fn find_sub(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    haystack.get(from..)?.find(needle).map(|p| p + from)
}

/// ASCII case-insensitive substring search.
fn find_ci(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let h = haystack.as_bytes();
    let nd = needle.as_bytes();
    if nd.is_empty() || from >= h.len() {
        return None;
    }
    let end = h.len().checked_sub(nd.len())?;
    (from..=end).find(|&i| h[i..i + nd.len()].eq_ignore_ascii_case(nd))
}

/// Decode &#123; &#xAB; and common named entities.
pub fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        // entity ends at ';' within a short window (byte scan is safe:
        // ';' is ASCII, so the found index is always a char boundary)
        let semi = tail
            .as_bytes()
            .iter()
            .take(32)
            .position(|&b| b == b';');
        match semi {
            Some(end) => {
                let entity = &tail[1..end];
                match decode_entity(entity) {
                    Some(decoded) => {
                        out.push_str(&decoded);
                        rest = &tail[end + 1..];
                    }
                    None => {
                        out.push('&');
                        rest = &tail[1..];
                    }
                }
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn decode_entity(entity: &str) -> Option<String> {
    if let Some(num) = entity.strip_prefix('#') {
        let code = if let Some(hex) =
            num.strip_prefix('x').or_else(|| num.strip_prefix('X'))
        {
            u32::from_str_radix(hex, 16).ok()?
        } else {
            num.parse::<u32>().ok()?
        };
        return char::from_u32(code).map(|c| c.to_string());
    }
    let ch = match entity {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" => "\u{00a0}",
        "copy" => "©",
        "reg" => "®",
        "trade" => "™",
        "mdash" => "—",
        "ndash" => "–",
        "hellip" => "…",
        "laquo" => "«",
        "raquo" => "»",
        "ldquo" => "\u{201c}",
        "rdquo" => "\u{201d}",
        "lsquo" => "\u{2018}",
        "rsquo" => "\u{2019}",
        "larr" => "←",
        "rarr" => "→",
        "uarr" => "↑",
        "darr" => "↓",
        "harr" => "↔",
        "times" => "×",
        "divide" => "÷",
        "middot" => "·",
        "bull" => "•",
        "sect" => "§",
        "para" => "¶",
        "deg" => "°",
        "plusmn" => "±",
        "frac12" => "½",
        "frac14" => "¼",
        "sup2" => "²",
        "sup3" => "³",
        "micro" => "µ",
        "euro" => "€",
        "pound" => "£",
        "yen" => "¥",
        "cent" => "¢",
        "dagger" => "†",
        "Dagger" => "‡",
        "permil" => "‰",
        "lsaquo" => "‹",
        "rsaquo" => "›",
        "shy" => "\u{00ad}",
        "ensp" => "\u{2002}",
        "emsp" => "\u{2003}",
        "thinsp" => "\u{2009}",
        "zwnj" => "\u{200c}",
        "zwj" => "\u{200d}",
        _ => return None,
    };
    Some(ch.to_string())
}
