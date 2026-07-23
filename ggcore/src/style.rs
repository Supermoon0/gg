//! Style computation: hash-bucket rule index + cascade + inheritance.

use std::collections::HashMap;

use crate::css::{CssParser, Rule, Simple};
use crate::dom::Document;

const INHERITED: &[(&str, &str)] = &[
    ("font-size", "16px"),
    ("font-style", "normal"),
    ("font-weight", "normal"),
    ("font-family", "default"),
    ("color", "black"),
    ("text-align", "left"),
    ("white-space", "normal"),
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

struct RuleIndex<'r> {
    by_id: HashMap<&'r str, Vec<usize>>,
    by_class: HashMap<&'r str, Vec<usize>>,
    by_tag: HashMap<&'r str, Vec<usize>>,
    universal: Vec<usize>,
    rules: &'r [Rule],
}

impl<'r> RuleIndex<'r> {
    fn new(rules: &'r [Rule]) -> Self {
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

    fn matching(&self, doc: &Document, node_idx: usize) -> Vec<usize> {
        let node = &doc.nodes[node_idx];
        let mut candidates: Vec<usize> = Vec::new();
        if let Some(tag) = node.tag.as_deref() {
            if let Some(v) = self.by_tag.get(tag) {
                candidates.extend_from_slice(v);
            }
        }
        for cls in &node.classes {
            if let Some(v) = self.by_class.get(cls.as_str()) {
                candidates.extend_from_slice(v);
            }
        }
        if let Some(id) = node.attr("id") {
            if let Some(v) = self.by_id.get(id) {
                candidates.extend_from_slice(v);
            }
        }
        candidates.extend_from_slice(&self.universal);

        let mut matched: Vec<usize> = candidates
            .into_iter()
            .filter(|&o| self.rules[o].selector.matches(doc, node_idx))
            .collect();
        // cascade: sort by (origin, specificity, source order) —
        // must mirror compute_styles' global sort
        matched.sort_by_key(|&o| {
            (self.rules[o].origin, self.rules[o].selector.specificity, o)
        });
        matched
    }
}

/// Parse css sources in cascade order and compute styles for every node.
pub fn compute_styles(doc: &mut Document, css_sources: &[String]) {
    compute_styles_vw(doc, css_sources, 1280.0);
}

/// As `compute_styles`, with an explicit viewport width for @media.
pub fn compute_styles_vw(
    doc: &mut Document,
    css_sources: &[String],
    viewport_width: f64,
) {
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
    let pseudo_rules: Vec<Rule> = {
        let mut ps = Vec::new();
        rules.retain_mut(|r| {
            if r.selector.pseudo.is_some() {
                ps.push(Rule {
                    selector: r.selector.clone(),
                    decls: std::mem::take(&mut r.decls),
                    origin: r.origin,
                });
                false
            } else {
                true
            }
        });
        ps
    };
    let index = RuleIndex::new(&rules);
    let root = doc.root;
    let default_style: HashMap<String, String> = INHERITED
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    style_node(
        doc, &index, &pseudo_rules, root, &default_style, 16.0,
        &HashMap::new(),
    );
}

/// Strip quotes from a CSS `content` string; None when the rule makes
/// no box (`none`/`normal`) or the value form is unsupported.
fn content_text(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() || t == "none" || t == "normal" {
        return None;
    }
    let b = t.as_bytes();
    if b.len() >= 2
        && (b[0] == b'"' || b[0] == b'\'')
        && b[b.len() - 1] == b[0]
    {
        return Some(t[1..t.len() - 1].to_string());
    }
    Some(String::new()) // attr()/counters: render an empty box
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
                parent_vars,
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
        for order in index.matching(doc, idx) {
            for (prop, value) in &index.rules[order].decls {
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

    doc.nodes[idx].style = style;

    let children = doc.nodes[idx].children.clone();
    let my_style = doc.nodes[idx].style.clone();
    // the root element establishes the `rem` unit for the whole subtree
    let child_root_px = if idx == doc.root {
        parse_px(
            my_style.get("font-size").map(String::as_str).unwrap_or("16px"),
            16.0,
        )
    } else {
        root_px
    };
    for child in children {
        style_node(
            doc, index, pseudo_rules, child, &my_style, child_root_px, vars,
        );
    }

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
    for which in [0u8, 1u8] {
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
        let mut any = false;
        for r in pseudo_rules {
            if r.selector.pseudo != Some(which)
                || !r.selector.matches(doc, idx)
            {
                continue;
            }
            any = true;
            for (prop, value) in &r.decls {
                apply(&mut style, prop, value);
            }
        }
        if !any {
            continue;
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
        let Some(text) =
            style.get("content").and_then(|c| content_text(c))
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
}

// --- CSS custom properties (var) — mirrors Python style.py ------------

const VAR_MAX_DEPTH: u32 = 16;

fn ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c >= 0x80
}

/// Next var(...) in value: (open_idx, end_idx_past_paren, inner).
fn find_var(value: &str, from: usize) -> Option<(usize, usize, &str)> {
    let b = value.as_bytes();
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
    if prop == "margin" {
        expand_box(style, "margin", value);
    } else if prop == "padding" {
        expand_box(style, "padding", value);
    } else if prop == "border" {
        let mut width: Option<f64> = None;
        let mut color: Option<&str> = None;
        for part in value.split_whitespace() {
            let p = part.to_ascii_lowercase();
            if p == "none" || p == "hidden" {
                width = Some(0.0);
            } else if matches!(p.as_str(), "solid" | "dashed" | "dotted"
                | "double" | "groove" | "ridge" | "inset" | "outset")
            {
                continue;
            } else if let Some(w) = parse_size(&p) {
                width = Some(w);
            } else {
                color = Some(part);
            }
        }
        match width {
            Some(w) => {
                style.insert("border-width".into(), px_string(w));
            }
            None => {
                style
                    .entry("border-width".into())
                    .or_insert_with(|| "1px".into());
            }
        }
        if let Some(c) = color {
            style.insert("border-color".into(), c.to_string());
        }
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
        style.insert(prop.to_string(), value.to_string());
    }
}
