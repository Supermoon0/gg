//! Style computation: hash-bucket rule index + cascade + inheritance.

use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use crate::css::{CssParser, Rule, Simple};
use crate::dom::{bloom_slot, Document, BLOOM_WORDS};

/// (property, initial value) for every property that inherits.
///
/// This list was eight entries long, which meant `line-height: 2` on
/// the body reached nothing below it — one of the most common lines of
/// CSS anyone writes. Same for letter-spacing, text-transform,
/// list-style-type and the rest: they were parsed, stored on the
/// element that declared them, and never seen by a descendant.
const INHERITED: &[(&str, &str)] = &[
    ("font-size", "16px"),
    ("font-style", "normal"),
    ("font-weight", "normal"),
    ("font-family", "default"),
    ("color", "black"),
    // "start", not "left": the initial value is writing-mode relative,
    // and spelling it "left" made `direction: rtl` align its text to
    // the left because a physical keyword has nothing to resolve.
    ("text-align", "start"),
    ("white-space", "normal"),
    ("visibility", "visible"),
    ("line-height", "normal"),
    ("letter-spacing", "normal"),
    ("word-spacing", "normal"),
    ("text-transform", "none"),
    ("text-indent", "0"),
    // Left empty rather than "disc": <ol> and <ul> pick different
    // markers by tag, and paint has to be able to tell an author's
    // `list-style-type: disc` from an initial value nobody wrote —
    // otherwise every ordered list on every page draws bullets.
    ("list-style-type", ""),
    ("cursor", "auto"),
    ("direction", "ltr"),
    ("word-break", "normal"),
    ("overflow-wrap", "normal"),
    ("font-variant", "normal"),
    ("text-shadow", "none"),
    // CSS Text 4 split white-space into these two. Both spellings of
    // the wrapping half are still in use, and both are inherited, so
    // a text node has to be able to see them the way it sees
    // white-space itself. Their initial value is left empty rather
    // than spelled out ("collapse"/"wrap"), because layout has to be
    // able to tell "the author set this" from "nobody set this": the
    // shorthand is not expanded here, so `white-space: pre` alone
    // would otherwise be overruled by an initial value nobody wrote.
    ("white-space-collapse", ""),
    ("text-wrap-mode", ""),
    ("text-wrap", ""),
];

fn parse_px(value: &str, default: f64) -> f64 {
    let v = value.trim();
    let v = v.strip_suffix("px").unwrap_or(v);
    v.trim()
        .parse::<f64>()
        .ok()
        .filter(|x| x.is_finite())
        .unwrap_or(default)
}

/// Bucket key: rightmost compound's most selective simple selector.
enum BucketKey<'r> {
    Id(&'r str),
    Class(&'r str),
    Tag(&'r str),
    Universal,
}

fn bucket_key(rule: &Rule) -> BucketKey<'_> {
    let compound = rule.selector.chain.last().unwrap();
    for p in compound {
        if let Simple::Id(id) = p {
            return BucketKey::Id(id);
        }
    }
    for p in compound {
        if let Simple::Class(c) = p {
            return BucketKey::Class(c);
        }
    }
    for p in compound {
        if let Simple::Tag(t) = p {
            return BucketKey::Tag(t);
        }
    }
    BucketKey::Universal
}

pub(crate) struct RuleIndex<'r> {
    by_id: HashMap<&'r str, Vec<usize>>,
    by_class: HashMap<&'r str, Vec<usize>>,
    by_tag: HashMap<&'r str, Vec<usize>>,
    universal: Vec<usize>,
    rules: &'r [Rule],
}

impl<'r> RuleIndex<'r> {
    pub(crate) fn new(rules: &'r [Rule]) -> Self {
        let mut idx = RuleIndex {
            by_id: HashMap::new(),
            by_class: HashMap::new(),
            by_tag: HashMap::new(),
            universal: Vec::new(),
            rules,
        };
        for (order, rule) in rules.iter().enumerate() {
            match bucket_key(rule) {
                BucketKey::Id(k) => {
                    idx.by_id.entry(k).or_default().push(order)
                }
                BucketKey::Class(k) => {
                    idx.by_class.entry(k).or_default().push(order)
                }
                BucketKey::Tag(k) => {
                    idx.by_tag.entry(k).or_default().push(order)
                }
                BucketKey::Universal => idx.universal.push(order),
            }
        }
        idx
    }

    /// Rules whose rightmost compound could match this node, by bucket.
    /// Split out of `matching` so the perf harness can measure selector
    /// interpretation without the caller's allocations.
    fn gather_candidates(
        &self,
        doc: &Document,
        node_idx: usize,
        out: &mut Vec<usize>,
    ) {
        out.clear();
        let node = &doc.nodes[node_idx];
        if let Some(tag) = node.tag.as_deref() {
            if let Some(v) = self.by_tag.get(tag) {
                out.extend_from_slice(v);
            }
        }
        for cls in &node.classes {
            if let Some(v) = self.by_class.get(cls.as_str()) {
                out.extend_from_slice(v);
            }
        }
        if let Some(id) = node.attr("id") {
            if let Some(v) = self.by_id.get(id) {
                out.extend_from_slice(v);
            }
        }
        out.extend_from_slice(&self.universal);
    }

    /// Rules matching this node, in cascade order, into `matched`.
    /// Both buffers are caller-owned so a whole tree walk can share one
    /// pair instead of allocating two Vecs per element.
    pub(crate) fn matching_into(
        &self,
        doc: &Document,
        node_idx: usize,
        candidates: &mut Vec<usize>,
        matched: &mut Vec<usize>,
    ) {
        self.gather_candidates(doc, node_idx, candidates);
        matched.clear();
        for &o in candidates.iter() {
            if self.rules[o].selector.matches(doc, node_idx) {
                matched.push(o);
            }
        }
        // `rules` is pre-sorted by (origin, specificity) in
        // `parse_sheets`, so ascending rule index already IS cascade
        // order — origin, then specificity, then source order. Sorting
        // the indices directly avoids re-reading every matched rule to
        // rebuild a sort key.
        matched.sort_unstable();
    }

    /// Allocating convenience wrapper (kept for callers that want an
    /// owned result; the style pass uses `matching_into`).
    pub(crate) fn matching(
        &self,
        doc: &Document,
        node_idx: usize,
    ) -> Vec<usize> {
        let mut candidates = Vec::new();
        let mut matched = Vec::new();
        self.matching_into(doc, node_idx, &mut candidates, &mut matched);
        matched
    }
}

/// Parse css sources in cascade order and compute styles for every node.
pub fn compute_styles(doc: &mut Document, css_sources: &[String]) {
    compute_styles_vw(doc, css_sources, 1280.0);
}

thread_local! {
    /// The last stylesheet set parsed, keyed by content + viewport.
    ///
    /// Every restyle used to re-parse every sheet from source: hovering
    /// a link re-parsed the page's entire CSS. Parsing is ~7% of a cold
    /// style pass and 100% wasted on a restyle, which is the case that
    /// has to hit 60fps. The honest fix is a caller-held `Stylesheet`
    /// handle (it would also survive across documents); this keeps the
    /// existing API while making a restyle free.
    static PARSED: RefCell<Option<(u64, Vec<Rule>, Vec<Rule>)>> =
        const { RefCell::new(None) };
}

fn sheet_key(css_sources: &[String], viewport_width: f64) -> u64 {
    let mut h = DefaultHasher::new();
    css_sources.len().hash(&mut h);
    for src in css_sources {
        src.hash(&mut h);
    }
    // @media is resolved at parse time, so the viewport is part of the
    // identity of the parsed result.
    viewport_width.to_bits().hash(&mut h);
    h.finish()
}

/// Parse the sources into (normal rules, ::before/::after rules).
pub(crate) fn parse_sheets(
    css_sources: &[String],
    viewport_width: f64,
) -> (Vec<Rule>, Vec<Rule>) {
    let mut rules: Vec<Rule> = Vec::new();
    for (si, src) in css_sources.iter().enumerate() {
        let mut parser = CssParser::new(src);
        parser.set_viewport(viewport_width);
        let mut parsed = parser.parse();
        if si == 0 {
            // the first source is the UA sheet (see native.py)
            for r in &mut parsed {
                r.origin = 0;
            }
        }
        rules.extend(parsed);
    }
    // stable sort keeps source order among equal keys; origin outranks
    // specificity so author resets can override the UA sheet
    rules.sort_by_key(|r| (r.origin, r.selector.specificity));

    // ::before/::after rules style synthesized children — they must
    // not participate in normal matching
    let mut pseudo_rules = Vec::new();
    rules.retain_mut(|r| {
        if r.selector.pseudo.is_some() {
            pseudo_rules.push(Rule {
                selector: r.selector.clone(),
                decls: std::mem::take(&mut r.decls),
                origin: r.origin,
            });
            false
        } else {
            true
        }
    });
    (rules, pseudo_rules)
}

/// Drop the parsed-stylesheet cache (so a benchmark can time a cold
/// pass; nothing in the engine needs this).
#[cfg(test)]
pub(crate) fn clear_stylesheet_cache() {
    PARSED.with(|c| *c.borrow_mut() = None);
}

/// As `compute_styles`, with an explicit viewport width for @media.
pub fn compute_styles_vw(
    doc: &mut Document,
    css_sources: &[String],
    viewport_width: f64,
) {
    let key = sheet_key(css_sources, viewport_width);
    PARSED.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.as_ref().map_or(true, |(k, _, _)| *k != key) {
            let (rules, pseudo) = parse_sheets(css_sources, viewport_width);
            *cache = Some((key, rules, pseudo));
        }
        let (_, rules, pseudo_rules) = cache.as_ref().unwrap();

        fill_ancestor_bloom(doc);
        let index = RuleIndex::new(rules);
        let root = doc.root;
        let default_style: HashMap<String, String> = INHERITED
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let mut scratch = Scratch::default();
        style_node(
            doc, &index, pseudo_rules, root, &default_style, 16.0,
            &HashMap::new(), &mut scratch,
        );
    });
}

/// Record every ancestor's tag/id/class names into each node's bloom
/// filter, so `css::matches` can reject a descendant selector without
/// walking to the root.
pub(crate) fn fill_ancestor_bloom(doc: &mut Document) {
    // All-ones means "unknown". Nodes not reachable from the root
    // (detached subtrees querySelector can still see) keep it, so they
    // are never rejected on the strength of a filter we never built.
    doc.ancestor_bloom.clear();
    doc.ancestor_bloom
        .resize(doc.nodes.len(), [u64::MAX; BLOOM_WORDS]);

    let mut stack = vec![(doc.root, [0u64; BLOOM_WORDS])];
    while let Some((idx, from_ancestors)) = stack.pop() {
        doc.ancestor_bloom[idx] = from_ancestors;
        let mut with_me = from_ancestors;
        let node = &doc.nodes[idx];
        // must stay in step with what `compound_matches` compares:
        // exact tag / class / id strings
        if let Some(tag) = node.tag.as_deref() {
            let (w, bit) = bloom_slot(tag);
            with_me[w] |= bit;
        }
        for class in &node.classes {
            let (w, bit) = bloom_slot(class);
            with_me[w] |= bit;
        }
        if let Some(id) = node.attr("id") {
            let (w, bit) = bloom_slot(id);
            with_me[w] |= bit;
        }
        for &child in &node.children {
            stack.push((child, with_me));
        }
    }
    doc.ancestor_bloom_version = doc.version;
}

/// Buffers reused across the whole tree walk. `matching` used to
/// allocate two Vecs per element; the matched set is consumed before
/// the recursion, so one pair for the entire pass is enough.
#[derive(Default)]
struct Scratch {
    candidates: Vec<usize>,
    matched: Vec<usize>,
}

/// Strip quotes from a CSS `content` string; None when the rule makes
/// no box (`none`/`normal`) or the value form is unsupported.
/// The text a `content` value generates, or None for no box at all.
///
/// A content value is a *list*: `content: "[" attr(href) "]"` is three
/// components concatenated. Quoted strings contribute their text and
/// `attr()` contributes the element's attribute (empty when absent,
/// which is what the spec says and what keeps a missing attribute from
/// removing the box). Counters and images still contribute nothing —
/// the box is generated but empty, which is what it was doing for
/// every non-string value before.
fn content_text(raw: &str, attrs: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() || t == "none" || t == "normal" {
        return None;
    }
    let mut out = String::new();
    let b: Vec<char> = t.chars().collect();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '"' || c == '\'' {
            let mut j = i + 1;
            while j < b.len() && b[j] != c {
                if b[j] != '\\' || j + 1 >= b.len() {
                    out.push(b[j]);
                    j += 1;
                    continue;
                }
                // a backslash starts either a character escape or a
                // unicode one — `content: "\\e903"` is an icon-font
                // codepoint, and pushing the digits as text draws
                // "e903" on the page where a glyph belongs
                let mut k = j + 1;
                let mut hex = String::new();
                while k < b.len() && hex.len() < 6 && b[k].is_ascii_hexdigit()
                {
                    hex.push(b[k]);
                    k += 1;
                }
                if hex.is_empty() {
                    out.push(b[j + 1]);
                    j += 2;
                    continue;
                }
                // one optional whitespace terminates the escape
                if k < b.len() && b[k].is_whitespace() {
                    k += 1;
                }
                if let Some(ch) = u32::from_str_radix(&hex, 16)
                    .ok()
                    .and_then(char::from_u32)
                {
                    out.push(ch);
                }
                j = k;
            }
            i = j + 1;
            continue;
        }
        // a function or keyword: read to its end
        let start = i;
        while i < b.len() && !b[i].is_whitespace() && b[i] != '(' {
            i += 1;
        }
        let name: String =
            b[start..i].iter().collect::<String>().to_ascii_lowercase();
        if i < b.len() && b[i] == '(' {
            let mut depth = 0;
            let arg_start = i + 1;
            while i < b.len() {
                if b[i] == '(' {
                    depth += 1;
                } else if b[i] == ')' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                i += 1;
            }
            let inner: String = b[arg_start..i.min(b.len())].iter().collect();
            i += 1;
            if name == "attr" {
                // attr(name) or attr(name type?, fallback)
                let (head, fallback) = match inner.split_once(',') {
                    Some((h, f)) => (h, f.trim().trim_matches(['"', '\''])),
                    None => (inner.as_str(), ""),
                };
                let key = head
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                out.push_str(
                    &attrs(&key).unwrap_or_else(|| fallback.to_string()),
                );
            }
        }
    }
    Some(out)
}

#[allow(clippy::too_many_arguments)]
fn style_node(
    doc: &mut Document,
    index: &RuleIndex,
    pseudo_rules: &[Rule],
    idx: usize,
    parent_style: &HashMap<String, String>,
    root_px: f64,
    parent_vars: &HashMap<String, String>,
    scratch: &mut Scratch,
) {
    // synthesized children keep the style their host computed for
    // them; only their text child needs the inheritance pass
    if doc.nodes[idx].tag.as_deref()
        == Some("::before")
        || doc.nodes[idx].tag.as_deref() == Some("::after")
    {
        let my_style = doc.nodes[idx].style.clone();
        let children = doc.nodes[idx].children.clone();
        for child in children {
            style_node(
                doc, index, pseudo_rules, child, &my_style, root_px,
                parent_vars, scratch,
            );
        }
        return;
    }
    let mut style: HashMap<String, String> = HashMap::with_capacity(12);

    // 1. inherited defaults
    for (prop, default) in INHERITED {
        let v = parent_style
            .get(*prop)
            .cloned()
            .unwrap_or_else(|| default.to_string());
        style.insert(prop.to_string(), v);
    }

    if doc.nodes[idx].is_element() {
        // 2. cascade via rule index
        index.matching_into(
            doc, idx, &mut scratch.candidates, &mut scratch.matched,
        );
        for k in 0..scratch.matched.len() {
            for (prop, value) in &index.rules[scratch.matched[k]].decls {
                apply(&mut style, prop, value);
            }
        }
        // 3. inline style attribute wins
        if let Some(inline) = doc.nodes[idx].attr("style") {
            let inline = inline.to_string();
            for (prop, value) in CssParser::new(&inline).body() {
                apply(&mut style, &prop, &value);
            }
        }
    }

    // 3.5 custom properties: collect --* declarations into the
    // inherited variable scope (copy-on-write — nodes that define none
    // share the parent's map), then substitute var() references. A
    // failed substitution is "invalid at computed-value time":
    // inherited properties fall back to the parent's value, others are
    // dropped. Mirrors Python style._style.
    let own: Vec<String> = style
        .keys()
        .filter(|k| k.starts_with("--"))
        .cloned()
        .collect();
    let vars_storage;
    let vars: &HashMap<String, String> = if own.is_empty() {
        parent_vars
    } else {
        let mut merged = parent_vars.clone();
        for prop in own {
            if let Some(v) = style.remove(&prop) {
                merged.insert(prop, v);
            }
        }
        vars_storage = merged;
        &vars_storage
    };
    let needs_vars: Vec<String> = style
        .iter()
        .filter(|(_, v)| find_var(v, 0).is_some())
        .map(|(k, _)| k.clone())
        .collect();
    for prop in needs_vars {
        let resolved = resolve_var_refs(&style[&prop], vars, 0);
        match resolved {
            Some(r) if !r.trim().is_empty() => {
                style.insert(prop, r.trim().to_string());
            }
            _ => {
                if let Some((_, default)) =
                    INHERITED.iter().find(|(k, _)| *k == prop)
                {
                    let v = parent_style
                        .get(&prop)
                        .cloned()
                        .unwrap_or_else(|| default.to_string());
                    style.insert(prop, v);
                } else {
                    style.remove(&prop);
                }
            }
        }
    }

    // 4. resolve relative font sizes against the parent
    let parent_px = parse_px(
        parent_style
            .get("font-size")
            .map(String::as_str)
            .unwrap_or("16px"),
        16.0,
    );
    let fs = style.get("font-size").cloned().unwrap_or_default();
    let resolved = if let Some(rem) = fs.strip_suffix("rem") {
        // must be checked before "em" (its suffix); rem is relative to
        // the root element's font-size, not a fixed 16px — pages like
        // naver set `html { font-size: 10px }` so 1rem == 10px.
        Some(root_px * rem.trim().parse::<f64>().unwrap_or(1.0))
    } else if let Some(pct) = fs.strip_suffix('%') {
        Some(parent_px * pct.trim().parse::<f64>().unwrap_or(100.0) / 100.0)
    } else if let Some(em) = fs.strip_suffix("em") {
        Some(parent_px * em.trim().parse::<f64>().unwrap_or(1.0))
    } else if fs.ends_with("px") {
        None
    } else {
        Some(match fs.as_str() {
            "xx-small" => 9.0,
            "x-small" => 10.0,
            "small" => 13.0,
            "medium" => 16.0,
            "large" => 18.0,
            "x-large" => 24.0,
            "xx-large" => 32.0,
            _ => parent_px,
        })
    };
    if let Some(px) = resolved {
        style.insert("font-size".to_string(), px_string(px));
    }

    // the root element establishes the `rem` unit for the whole subtree
    let child_root_px = if idx == doc.root {
        parse_px(
            style.get("font-size").map(String::as_str).unwrap_or("16px"),
            16.0,
        )
    } else {
        root_px
    };
    // `style` stays owned here across the recursion: the children only
    // ever need a shared reference to it, so storing it into the node
    // first and cloning it back out was one full HashMap deep copy per
    // element. The child list is walked by index rather than cloned for
    // the same reason — and it must stay in the tree while the children
    // are styled, because sibling and :nth-child selectors read it.
    for k in 0..doc.nodes[idx].children.len() {
        let child = doc.nodes[idx].children[k];
        style_node(
            doc, index, pseudo_rules, child, &style, child_root_px, vars,
            scratch,
        );
    }
    doc.nodes[idx].style = style;

    // synthesize ::before/::after children from matching pseudo rules
    if doc.nodes[idx].is_element() {
        synthesize_pseudos(doc, pseudo_rules, idx, vars);
    }
}

/// Create/update the ::before and ::after children of `idx` from the
/// pseudo rules whose base selector matches it. Mirrored in Python
/// style.py — the two engines must synthesize identical nodes.
fn synthesize_pseudos(
    doc: &mut Document,
    pseudo_rules: &[Rule],
    idx: usize,
    vars: &HashMap<String, String>,
) {
    if pseudo_rules.is_empty() {
        return;
    }
    for which in [0u8, 1u8] {
        // Probe before building anything. The inherited style map below
        // costs a HashMap and sixteen Strings, and hardly any element
        // has a ::before or ::after — paying for it on every element of
        // every page was most of what this function did.
        if !pseudo_rules.iter().any(|r| {
            r.selector.pseudo == Some(which)
                && r.selector.matches(doc, idx)
        }) {
            continue;
        }
        let mut style: HashMap<String, String> = HashMap::new();
        // inherited properties come from the host element
        for (prop, default) in INHERITED {
            let v = doc.nodes[idx]
                .style
                .get(*prop)
                .cloned()
                .unwrap_or_else(|| default.to_string());
            style.insert(prop.to_string(), v);
        }
        for r in pseudo_rules {
            if r.selector.pseudo != Some(which)
                || !r.selector.matches(doc, idx)
            {
                continue;
            }
            for (prop, value) in &r.decls {
                apply(&mut style, prop, value);
            }
        }
        // var() substitution with the host's variable scope
        let needs: Vec<String> = style
            .iter()
            .filter(|(_, v)| find_var(v, 0).is_some())
            .map(|(k, _)| k.clone())
            .collect();
        for prop in needs {
            match resolve_var_refs(&style[&prop], vars, 0) {
                Some(r) if !r.trim().is_empty() => {
                    style.insert(prop, r.trim().to_string());
                }
                _ => {
                    style.remove(&prop);
                }
            }
        }
        let host_attrs = |name: &str| {
            doc.nodes[idx].attr(name).map(|v| v.to_string())
        };
        let Some(text) = style
            .get("content")
            .and_then(|c| content_text(c, &host_attrs))
        else {
            continue; // no content -> no box
        };
        if !style.contains_key("display") {
            style.insert("display".into(), "inline".into());
        }
        let tag = if which == 0 { "::before" } else { "::after" };
        // reuse an existing synthesized child (restyle pass)
        let existing = doc.nodes[idx]
            .children
            .iter()
            .copied()
            .find(|&c| doc.nodes[c].tag.as_deref() == Some(tag));
        let pidx = match existing {
            Some(p) => p,
            None => {
                let p = doc.new_element(tag.to_string(), Vec::new(), None);
                doc.nodes[p].parent = Some(idx);
                if which == 0 {
                    doc.nodes[idx].children.insert(0, p);
                } else {
                    doc.nodes[idx].children.push(p);
                }
                p
            }
        };
        doc.nodes[pidx].style = style;
        if !text.is_empty()
            && doc.nodes[pidx].children.is_empty()
        {
            doc.new_text(text, pidx);
        }
    }
    // Synthesizing a pseudo bumps `version`, which would retire the
    // ancestor filter for the rest of the pass. Nothing here changes an
    // existing node's ancestors, so the filter stays valid: mark the
    // new nodes unknown and re-adopt the version.
    if doc.ancestor_bloom.len() < doc.nodes.len() {
        doc.ancestor_bloom
            .resize(doc.nodes.len(), [u64::MAX; BLOOM_WORDS]);
    }
    doc.ancestor_bloom_version = doc.version;
}

// --- CSS custom properties (var) — mirrors Python style.py ------------

const VAR_MAX_DEPTH: u32 = 16;

fn ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c >= 0x80
}

/// Next var(...) in value: (open_idx, end_idx_past_paren, inner).
fn find_var(value: &str, from: usize) -> Option<(usize, usize, &str)> {
    let b = value.as_bytes();
    // Fast reject: this runs for every property of every element, and
    // a var() reference always contains '(' — most values contain none,
    // so one scan beats the case-insensitive 4-byte compare per offset.
    if from >= b.len() || !b[from..].contains(&b'(') {
        return None;
    }
    let mut i = from;
    while i + 4 <= b.len() {
        if b[i..i + 4].eq_ignore_ascii_case(b"var(")
            && (i == 0 || !ident_byte(b[i - 1]))
        {
            let mut depth = 1usize;
            let mut j = i + 4;
            while j < b.len() && depth > 0 {
                match b[j] {
                    b'(' => depth += 1,
                    b')' => depth -= 1,
                    _ => {}
                }
                j += 1;
            }
            if depth > 0 {
                return None; // unbalanced — treat the rest as plain text
            }
            return Some((i, j, &value[i + 4..j - 1]));
        }
        i += 1;
    }
    None
}

/// Split "name" / "name, fallback" at the first top-level comma.
fn split_fallback(inner: &str) -> (&str, Option<&str>) {
    let b = inner.as_bytes();
    let mut depth = 0usize;
    for (k, &c) in b.iter().enumerate() {
        match c {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            b',' if depth == 0 => {
                return (&inner[..k], Some(&inner[k + 1..]));
            }
            _ => {}
        }
    }
    (inner, None)
}

/// Substitute every var() in value using vars. Returns None when the
/// value is invalid at computed-value time (undefined variable with no
/// fallback, or a reference cycle).
fn resolve_var_refs(
    value: &str,
    vars: &HashMap<String, String>,
    depth: u32,
) -> Option<String> {
    if depth > VAR_MAX_DEPTH {
        return None;
    }
    let mut out = String::new();
    let mut i = 0usize;
    loop {
        match find_var(value, i) {
            None => {
                out.push_str(&value[i..]);
                break;
            }
            Some((start, end, inner)) => {
                out.push_str(&value[i..start]);
                let (name, fallback) = split_fallback(inner);
                let key = name.trim().to_ascii_lowercase();
                let mut resolved = match vars.get(&key) {
                    Some(sub) if !sub.trim().is_empty() => {
                        resolve_var_refs(sub, vars, depth + 1)
                    }
                    _ => None,
                };
                if resolved.as_deref().map_or(true, |r| r.trim().is_empty())
                {
                    if let Some(fb) = fallback {
                        resolved =
                            resolve_var_refs(fb.trim(), vars, depth + 1);
                    }
                }
                match resolved {
                    Some(r) if !r.trim().is_empty() => out.push_str(&r),
                    _ => return None,
                }
                i = end;
            }
        }
    }
    Some(out)
}

/// `font: [style] [variant] [weight] <size>[/<line-height>] <family>`
///
/// The size is the pivot: everything before it is the optional
/// style/variant/weight prefix and everything after it is the family
/// list. The shorthand also resets the longhands it does not mention,
/// which is why `font: 25px/1 Ahem` has to write font-style and
/// font-weight back to normal rather than leaving whatever was there.
///
/// System font keywords (`caption`, `menu`, ...) name a font this
/// engine has no table for, so they are left alone rather than
/// resolved to a guess.
fn expand_font(style: &mut HashMap<String, String>, value: &str) {
    let v = value.trim();
    let lower = v.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "caption" | "icon" | "menu" | "message-box" | "small-caption"
            | "status-bar" | "inherit" | "initial" | "unset"
    ) {
        return;
    }
    // the size is the first token that starts with a digit or a dot,
    // or one of the absolute-size keywords
    let tokens: Vec<&str> = v.split_whitespace().collect();
    let size_at = tokens.iter().position(|t| {
        let s = t.trim_start_matches(['+', '-']);
        s.starts_with(|c: char| c.is_ascii_digit() || c == '.')
            || matches!(
                t.to_ascii_lowercase().as_str(),
                "xx-small" | "x-small" | "small" | "medium" | "large"
                    | "x-large" | "xx-large" | "larger" | "smaller"
            )
    });
    let Some(size_at) = size_at else { return };
    if size_at + 1 >= tokens.len() {
        return; // no family: not a valid font shorthand
    }
    let (size, line_height) = match tokens[size_at].split_once('/') {
        Some((s, lh)) => (s, Some(lh)),
        None => (tokens[size_at], None),
    };
    for (prop, initial) in [
        ("font-style", "normal"),
        ("font-variant", "normal"),
        ("font-weight", "normal"),
        ("line-height", "normal"),
    ] {
        style.insert(prop.into(), initial.into());
    }
    for token in &tokens[..size_at] {
        let t = token.to_ascii_lowercase();
        match t.as_str() {
            "italic" | "oblique" => {
                style.insert("font-style".into(), t);
            }
            "small-caps" => {
                style.insert("font-variant".into(), t);
            }
            "bold" | "bolder" | "lighter" | "100" | "200" | "300"
            | "400" | "500" | "600" | "700" | "800" | "900" => {
                style.insert("font-weight".into(), t);
            }
            _ => {}
        }
    }
    style.insert("font-size".into(), size.to_string());
    if let Some(lh) = line_height {
        if !lh.is_empty() {
            style.insert("line-height".into(), lh.to_string());
        }
    }
    style.insert("font-family".into(), tokens[size_at + 1..].join(" "));
}

fn expand_box(style: &mut HashMap<String, String>, prefix: &str, value: &str) {
    let parts: Vec<&str> = value.split_whitespace().collect();
    let (t, r, b, l) = match parts.len() {
        1 => (parts[0], parts[0], parts[0], parts[0]),
        2 => (parts[0], parts[1], parts[0], parts[1]),
        3 => (parts[0], parts[1], parts[2], parts[1]),
        4 => (parts[0], parts[1], parts[2], parts[3]),
        _ => return,
    };
    style.insert(format!("{prefix}-top"), t.into());
    style.insert(format!("{prefix}-right"), r.into());
    style.insert(format!("{prefix}-bottom"), b.into());
    style.insert(format!("{prefix}-left"), l.into());
}

fn expand_axis(
    style: &mut HashMap<String, String>,
    start: &str,
    end: &str,
    value: &str,
) {
    let parts: Vec<&str> = value.split_whitespace().collect();
    if let Some(first) = parts.first() {
        style.insert(start.into(), (*first).into());
        style.insert(end.into(), parts.get(1).unwrap_or(first).to_string());
    }
}

fn border_parts(value: &str) -> (Option<f64>, Option<String>, Option<&str>) {
    let mut width: Option<f64> = None;
    let mut line_style: Option<String> = None;
    let mut color: Option<&str> = None;
    for part in value.split_whitespace() {
        let p = part.to_ascii_lowercase();
        if matches!(p.as_str(), "none" | "hidden" | "solid" | "dashed"
            | "dotted" | "double" | "groove" | "ridge" | "inset"
            | "outset")
        {
            if p == "none" || p == "hidden" {
                width = Some(0.0);
            }
            line_style = Some(p);
        } else if let Some(w) = parse_size(&p) {
            width = Some(w);
        } else {
            color = Some(part);
        }
    }
    if width.is_none() && line_style.is_some() {
        width = Some(if matches!(line_style.as_deref(), Some("none" | "hidden")) {
            0.0
        } else {
            1.0
        });
    }
    (width, line_style, color)
}

fn set_border_side(
    style: &mut HashMap<String, String>,
    side: &str,
    width: Option<f64>,
    line_style: Option<&str>,
    color: Option<&str>,
) {
    if let Some(w) = width {
        style.insert(format!("border-{side}-width"), px_string(w));
    }
    if let Some(s) = line_style {
        style.insert(format!("border-{side}-style"), s.to_string());
    }
    if let Some(c) = color {
        style.insert(format!("border-{side}-color"), c.to_string());
    }
}

fn expand_border_box(
    style: &mut HashMap<String, String>,
    suffix: &str,
    value: &str,
) {
    let parts: Vec<&str> = value.split_whitespace().collect();
    let values = match parts.len() {
        1 => [parts[0], parts[0], parts[0], parts[0]],
        2 => [parts[0], parts[1], parts[0], parts[1]],
        3 => [parts[0], parts[1], parts[2], parts[1]],
        4 => [parts[0], parts[1], parts[2], parts[3]],
        _ => return,
    };
    for (side, part) in ["top", "right", "bottom", "left"]
        .iter().zip(values.iter())
    {
        style.insert(format!("border-{side}-{suffix}"), (*part).to_string());
    }
}

/// Mirror of Python's style.parse_size (default bases).
fn parse_size(value: &str) -> Option<f64> {
    let v = value.trim().to_ascii_lowercase();
    if matches!(v.as_str(), "auto" | "none" | "inherit" | "initial"
        | "unset" | "min-content" | "max-content" | "fit-content")
    {
        return None;
    }
    if let Some(n) = v.strip_suffix("px") {
        return n.trim().parse().ok().filter(|x: &f64| x.is_finite());
    }
    if let Some(n) = v.strip_suffix("rem") {
        return n.trim().parse::<f64>().ok().map(|x| x * 16.0)
            .filter(|x| x.is_finite());
    }
    if let Some(n) = v.strip_suffix("em") {
        return n.trim().parse::<f64>().ok().map(|x| x * 16.0)
            .filter(|x| x.is_finite());
    }
    if v.ends_with('%') || v.ends_with("vw") || v.ends_with("vh") {
        return Some(0.0);
    }
    v.parse().ok().filter(|x: &f64| x.is_finite())
}

fn px_string(value: f64) -> String {
    if !value.is_finite() {
        return "0px".to_string(); // parity with Python px_str()
    }
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}px", value as i64)
    } else {
        format!("{}px", value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::html;

    fn styled(css: &str, html_src: &str) -> Document {
        let mut doc = html::parse(html_src);
        compute_styles(
            &mut doc,
            &["".to_string(), css.to_string()],
        );
        doc
    }

    fn tag_style<'d>(
        doc: &'d Document,
        tag: &str,
    ) -> &'d HashMap<String, String> {
        let idx = (0..doc.nodes.len())
            .find(|&i| doc.nodes[i].tag.as_deref() == Some(tag))
            .unwrap();
        &doc.nodes[idx].style
    }

    #[test]
    fn var_resolves_from_root() {
        let doc = styled(":root{--c:#123456} p{color:var(--c)}", "<p>x</p>");
        assert_eq!(tag_style(&doc, "p")["color"], "#123456");
        assert!(!tag_style(&doc, "p").contains_key("--c"));
    }

    #[test]
    fn flex_shorthand_expands() {
        let doc = styled(
            "p{flex: 1} span{flex: 0 0 200px} b{flex: none}",
            "<p>x</p><span>y</span><b>z</b>",
        );
        let p = tag_style(&doc, "p");
        assert_eq!(p["flex-grow"], "1");
        assert_eq!(p["flex-basis"], "0");
        let s = tag_style(&doc, "span");
        assert_eq!(s["flex-grow"], "0");
        assert_eq!(s["flex-shrink"], "0");
        assert_eq!(s["flex-basis"], "200px");
        let b = tag_style(&doc, "b");
        assert_eq!(b["flex-grow"], "0");
        assert_eq!(b["flex-basis"], "auto");
    }

    #[test]
    fn logical_ltr_box_properties_expand() {
        let doc = styled(
            "p{margin-inline-start:20px;padding-inline:3px 5px;\
                inline-size:40px;inset-block-start:7px}",
            "<p>x</p>",
        );
        let p = tag_style(&doc, "p");
        assert_eq!(p["margin-left"], "20px");
        assert_eq!(p["padding-left"], "3px");
        assert_eq!(p["padding-right"], "5px");
        assert_eq!(p["width"], "40px");
        assert_eq!(p["top"], "7px");
    }

    #[test]
    fn physical_border_sides_expand_and_cascade_independently() {
        let doc = styled(
            ".g{border:none} textarea.g{border-bottom:8px solid transparent}",
            "<textarea class=g></textarea>",
        );
        let ta = tag_style(&doc, "textarea");
        assert_eq!(ta["border-top-width"], "0px");
        assert_eq!(ta["border-right-width"], "0px");
        assert_eq!(ta["border-bottom-width"], "8px");
        assert_eq!(ta["border-bottom-style"], "solid");
        assert_eq!(ta["border-bottom-color"], "transparent");
    }

    #[test]
    fn hover_and_focus_rules_follow_document_state() {
        let css = "p{color:black} p:hover{color:red} \
                   div:hover span{color:green} input:focus{color:blue}";
        let html_src = "<div><p>x</p><span>s</span></div><input>";
        // no hover/focus state: pseudo rules stay inert
        let doc = styled(css, html_src);
        assert_eq!(tag_style(&doc, "p")["color"], "black");
        assert!(tag_style(&doc, "span").get("color")
            .map(|c| c != "green").unwrap_or(true));
        // hovering <p>: its ancestors join the chain, so both the
        // subject rule and the ancestor-hover descendant rule fire
        let mut doc = html::parse(html_src);
        let p = (0..doc.nodes.len())
            .find(|&i| doc.nodes[i].tag.as_deref() == Some("p"))
            .unwrap();
        let mut cur = Some(p);
        while let Some(i) = cur {
            doc.hover_chain.push(i);
            cur = doc.nodes[i].parent;
        }
        let input = (0..doc.nodes.len())
            .find(|&i| doc.nodes[i].tag.as_deref() == Some("input"))
            .unwrap();
        doc.focused = Some(input);
        compute_styles(&mut doc, &["".to_string(), css.to_string()]);
        assert_eq!(tag_style(&doc, "p")["color"], "red");
        assert_eq!(tag_style(&doc, "span")["color"], "green");
        assert_eq!(tag_style(&doc, "input")["color"], "blue");
    }

    #[test]
    fn where_root_defines_vars() {
        let doc = styled(
            ":where(:root,:host){--bg:#0f0} p{background-color:var(--bg)}",
            "<p>x</p>",
        );
        assert_eq!(tag_style(&doc, "p")["background-color"], "#0f0");
    }

    #[test]
    fn chained_vars_and_fallback() {
        let doc = styled(
            "html{--a:var(--b)} :root{--b:red} \
             p{color:var(--a)} div{color:var(--nope, blue)}",
            "<div>y</div><p>x</p>",
        );
        assert_eq!(tag_style(&doc, "p")["color"], "red");
        assert_eq!(tag_style(&doc, "div")["color"], "blue");
    }

    #[test]
    fn unresolvable_inherited_falls_back_to_parent() {
        let doc = styled(
            "body{color:#222} p{color:var(--gone)}",
            "<body><p>x</p></body>",
        );
        assert_eq!(tag_style(&doc, "p")["color"], "#222");
    }

    #[test]
    fn unresolvable_non_inherited_is_dropped() {
        let doc = styled("p{background-color:var(--gone)}", "<p>x</p>");
        assert!(!tag_style(&doc, "p").contains_key("background-color"));
    }

    #[test]
    fn var_cycle_terminates_and_uses_fallback() {
        let doc = styled(
            ":root{--a:var(--b);--b:var(--a)} p{color:var(--a,#345678)}",
            "<p>x</p>",
        );
        assert_eq!(tag_style(&doc, "p")["color"], "#345678");
    }

    #[test]
    fn var_font_size_resolves_relative_units() {
        let doc = styled(":root{--fs:2em} p{font-size:var(--fs)}",
                         "<p>x</p>");
        assert_eq!(tag_style(&doc, "p")["font-size"], "32px");
    }

    #[test]
    fn rem_resolves_against_root_font_size_not_fixed_16() {
        // naver sets `html { font-size: 10px }`, so 1.3rem must be 13px
        // (not 20.8px against a hardcoded 16px root).
        let doc = styled(
            "html{font-size:10px} p{font-size:1.3rem} \
             b{font-size:1.5rem}",
            "<p>x<b>y</b></p>",
        );
        assert_eq!(tag_style(&doc, "p")["font-size"], "13px");
        assert_eq!(tag_style(&doc, "b")["font-size"], "15px");
        // em still resolves against the (inherited) parent font-size
        let doc = styled(
            "html{font-size:10px} p{font-size:2rem} b{font-size:1.5em}",
            "<p>x<b>y</b></p>",
        );
        assert_eq!(tag_style(&doc, "p")["font-size"], "20px");
        assert_eq!(tag_style(&doc, "b")["font-size"], "30px");
    }

    #[test]
    fn where_attr_alternatives_stay_inert_without_the_attribute() {
        let doc = styled(
            ":where([data-theme=dark]){--x:#000} p{color:var(--x,#eee)}",
            "<p>x</p>",
        );
        assert_eq!(tag_style(&doc, "p")["color"], "#eee");
    }

    #[test]
    fn pseudo_elements_synthesize_children() {
        let doc = styled(
            ".ico::before{content:\"\"; width:20px; height:20px; \
             background-image:url(s.png)} \
             p::after{content:\"!\"; color:red}",
            "<p class=ico>hi</p>",
        );
        let p = (0..doc.nodes.len())
            .find(|&i| doc.nodes[i].tag.as_deref() == Some("p"))
            .unwrap();
        let kids: Vec<&str> = doc.nodes[p]
            .children
            .iter()
            .map(|&c| doc.nodes[c].tag.as_deref().unwrap_or("#text"))
            .collect();
        assert_eq!(kids, vec!["::before", "#text", "::after"]);
        let before = doc.nodes[p].children[0];
        assert_eq!(doc.nodes[before].style["width"], "20px");
        assert!(doc.nodes[before].style["background-image"]
            .contains("s.png"));
        assert!(doc.nodes[before].children.is_empty()); // content:""
        let after = *doc.nodes[p].children.last().unwrap();
        assert_eq!(doc.nodes[after].style["color"], "red");
        let atext = doc.nodes[after].children[0];
        assert_eq!(doc.nodes[atext].text, "!");
        // restyle must not duplicate the synthesized children
        let mut doc = doc;
        compute_styles(
            &mut doc,
            &["".to_string(),
              ".ico::before{content:\"\"} p::after{content:\"!\"}"
                  .to_string()],
        );
        assert_eq!(doc.nodes[p].children.len(), 3);
    }

    #[test]
    fn sibling_and_child_combinators_match() {
        let css = "li+li{padding-left:26px} \
                   div>p{color:red} \
                   b~i{font-weight:bold} \
                   span+span::before{content:\"|\"}";
        let html_src = "<ul><li id=a>a</li><li id=b>b</li></ul>\
                        <div><section><p id=deep>x</p></section>\
                        <p id=direct>y</p></div>\
                        <b>1</b><u>2</u><i>3</i>\
                        <span id=s1>1</span><span id=s2>2</span>";
        let doc = styled(css, html_src);
        let by_id = |id: &str| {
            (0..doc.nodes.len())
                .find(|&i| doc.nodes[i].attr("id") == Some(id))
                .unwrap()
        };
        // li + li hits only the second list item
        assert!(!doc.nodes[by_id("a")].style.contains_key("padding-left"));
        assert_eq!(doc.nodes[by_id("b")].style["padding-left"], "26px");
        // div > p hits the direct child, not the deeper descendant
        assert_eq!(doc.nodes[by_id("direct")].style["color"], "red");
        assert!(doc.nodes[by_id("deep")].style.get("color")
            .map(|c| c != "red").unwrap_or(true));
        // b ~ i skips over the intervening <u>
        assert_eq!(tag_style(&doc, "i")["font-weight"], "bold");
        // A + B::before synthesizes only on the second span
        assert!(doc.nodes[by_id("s1")].children.iter().all(|&c| {
            doc.nodes[c].tag.as_deref() != Some("::before")
        }));
        assert!(doc.nodes[by_id("s2")].children.iter().any(|&c| {
            doc.nodes[c].tag.as_deref() == Some("::before")
        }));
    }

    #[test]
    fn content_none_makes_no_box() {
        let doc = styled("p::before{content:none; color:red}",
                         "<p>hi</p>");
        let p = (0..doc.nodes.len())
            .find(|&i| doc.nodes[i].tag.as_deref() == Some("p"))
            .unwrap();
        assert_eq!(doc.nodes[p].children.len(), 1); // text only
    }

    #[test]
    fn attribute_selectors_match() {
        let doc = styled(
            "[data-x=on]{color:red} [data-y]{color:blue} \
             [href^=\"https:\"]{color:green} [class~=big]{color:purple}",
            "<p data-x=on>a</p><div data-y=1>b</div>\
             <a href=\"https://x\">c</a><b class=\"a big\">d</b>",
        );
        assert_eq!(tag_style(&doc, "p")["color"], "red");
        assert_eq!(tag_style(&doc, "div")["color"], "blue");
        assert_eq!(tag_style(&doc, "a")["color"], "green");
        assert_eq!(tag_style(&doc, "b")["color"], "purple");
    }

    #[test]
    fn media_queries_evaluate_against_viewport() {
        // desktop viewport (default 1280): min-width applies, max-width
        // mobile rule does not
        let doc = styled(
            "p{color:black} \
             @media (min-width: 768px){p{color:red}} \
             @media (max-width: 600px){p{color:blue}}",
            "<p>x</p>",
        );
        assert_eq!(tag_style(&doc, "p")["color"], "red");
        // explicit narrow viewport flips it
        let mut d2 = html::parse("<p>x</p>");
        compute_styles_vw(
            &mut d2,
            &["".to_string(),
              "p{color:black} \
               @media (min-width: 768px){p{color:red}} \
               @media (max-width: 600px){p{color:blue}}".to_string()],
            500.0,
        );
        let pi = (0..d2.nodes.len())
            .find(|&i| d2.nodes[i].tag.as_deref() == Some("p"))
            .unwrap();
        assert_eq!(d2.nodes[pi].style["color"], "blue");
        // `and`, screen type, unknown feature don't drop the rule
        let doc = styled(
            "@media screen and (min-width: 100px){p{font-weight:bold}}",
            "<p>x</p>",
        );
        assert_eq!(tag_style(&doc, "p")["font-weight"], "bold");
        // a media condition written across lines (real sites format them
        // this way) must still evaluate its features — a desktop viewport
        // must NOT pick up a `max-width: 750px` mobile rule
        let doc = styled(
            "p{color:black}\n@media only screen\nand (min-width : 300px)\n\
             and (max-width : 750px) {\n  p{color:blue}\n}",
            "<p>x</p>",
        );
        assert_eq!(tag_style(&doc, "p")["color"], "black");
    }

    #[test]
    fn not_and_structural_selectors() {
        let doc = styled(
            "li:not(.skip){color:red} \
             li:first-child{font-weight:bold} \
             li:last-child{font-style:italic} \
             li:nth-child(2){text-align:center} \
             li:nth-child(even){white-space:nowrap}",
            "<ul><li>a</li><li class=skip>b</li><li>c</li></ul>",
        );
        let lis: Vec<usize> = (0..doc.nodes.len())
            .filter(|&i| doc.nodes[i].tag.as_deref() == Some("li"))
            .collect();
        // first li: :not(.skip) red, :first-child bold, nth-child(1) odd
        assert_eq!(doc.nodes[lis[0]].style["color"], "red");
        assert_eq!(doc.nodes[lis[0]].style["font-weight"], "bold");
        // second li has .skip: :not(.skip) does NOT apply
        assert_ne!(
            doc.nodes[lis[1]].style.get("color").map(String::as_str),
            Some("red")
        );
        // second li: nth-child(2) exact + even
        assert_eq!(doc.nodes[lis[1]].style["text-align"], "center");
        assert_eq!(doc.nodes[lis[1]].style["white-space"], "nowrap");
        // third li: :last-child italic
        assert_eq!(doc.nodes[lis[2]].style["font-style"], "italic");
    }

    #[test]
    fn attribute_selector_mismatch_is_ignored() {
        let doc = styled("[data-x=on]{color:red}", "<p data-x=off>a</p>");
        assert_eq!(tag_style(&doc, "p")["color"], "black");
    }
}

fn apply(style: &mut HashMap<String, String>, prop: &str, value: &str) {
    let value = value.trim();
    // Resolve the common logical box properties to the LTR physical box
    // model while declarations are applied, preserving cascade order.
    let prop = match prop {
        "margin-inline-start" => "margin-left",
        "margin-inline-end" => "margin-right",
        "margin-block-start" => "margin-top",
        "margin-block-end" => "margin-bottom",
        "padding-inline-start" => "padding-left",
        "padding-inline-end" => "padding-right",
        "padding-block-start" => "padding-top",
        "padding-block-end" => "padding-bottom",
        "inset-inline-start" => "left",
        "inset-inline-end" => "right",
        "inset-block-start" => "top",
        "inset-block-end" => "bottom",
        "inline-size" => "width",
        "block-size" => "height",
        "min-inline-size" => "min-width",
        "max-inline-size" => "max-width",
        "min-block-size" => "min-height",
        "max-block-size" => "max-height",
        other => other,
    };
    if prop == "margin-inline" {
        expand_axis(style, "margin-left", "margin-right", value);
    } else if prop == "margin-block" {
        expand_axis(style, "margin-top", "margin-bottom", value);
    } else if prop == "padding-inline" {
        expand_axis(style, "padding-left", "padding-right", value);
    } else if prop == "padding-block" {
        expand_axis(style, "padding-top", "padding-bottom", value);
    } else if prop == "inset-inline" {
        expand_axis(style, "left", "right", value);
    } else if prop == "inset-block" {
        expand_axis(style, "top", "bottom", value);
    } else if prop == "font" {
        expand_font(style, value);
    } else if prop == "margin" {
        expand_box(style, "margin", value);
    } else if prop == "padding" {
        expand_box(style, "padding", value);
    } else if prop == "border" {
        let (mut width, line_style, color) = border_parts(value);
        match width {
            Some(w) => {
                style.insert("border-width".into(), px_string(w));
            }
            None => {
                style
                    .entry("border-width".into())
                    .or_insert_with(|| "1px".into());
                width = Some(1.0);
            }
        }
        if let Some(ref s) = line_style {
            style.insert("border-style".into(), s.clone());
        }
        if let Some(c) = color {
            style.insert("border-color".into(), c.to_string());
        }
        for side in ["top", "right", "bottom", "left"] {
            set_border_side(
                style, side, width, line_style.as_deref(), color,
            );
        }
    } else if matches!(prop, "border-top" | "border-right"
        | "border-bottom" | "border-left")
    {
        let side = &prop[7..];
        let (width, line_style, color) = border_parts(value);
        set_border_side(style, side, width, line_style.as_deref(), color);
    } else if matches!(prop, "border-width" | "border-style" | "border-color") {
        let suffix = &prop[7..];
        style.insert(prop.into(), value.into());
        expand_border_box(style, suffix, value);
    } else if prop == "flex" {
        // flex: none | <grow> <shrink>? <basis>?
        if value.eq_ignore_ascii_case("none") {
            style.insert("flex-grow".into(), "0".into());
            style.insert("flex-shrink".into(), "0".into());
            style.insert("flex-basis".into(), "auto".into());
        } else {
            let mut nums: Vec<f64> = Vec::new();
            let mut basis: Option<&str> = None;
            for part in value.split_whitespace() {
                match part.parse::<f64>() {
                    Ok(n) => nums.push(n),
                    Err(_) => basis = Some(part),
                }
            }
            if let Some(&g) = nums.first() {
                style.insert("flex-grow".into(), g.to_string());
            }
            if let Some(&s) = nums.get(1) {
                style.insert("flex-shrink".into(), s.to_string());
            }
            match basis {
                Some(b) => {
                    style.insert("flex-basis".into(), b.to_string());
                }
                None if !nums.is_empty() => {
                    // "flex: 1" means basis 0 per spec
                    style.insert("flex-basis".into(), "0".into());
                }
                None => {}
            }
        }
    } else if prop == "font" {
        // too complex to fully parse; ignore rather than misrender
    } else {
        set(style, prop, value);
    }
}

/// Set a declaration, reusing the stored String when the property is
/// already present. An element matches dozens of rules that mostly
/// re-set the same handful of properties, so `insert(k.to_string(),
/// v.to_string())` was allocating (and immediately freeing) two Strings
/// per declaration for the whole cascade.
fn set(style: &mut HashMap<String, String>, prop: &str, value: &str) {
    match style.get_mut(prop) {
        Some(slot) => {
            slot.clear();
            slot.push_str(value);
        }
        None => {
            style.insert(prop.to_string(), value.to_string());
        }
    }
}
