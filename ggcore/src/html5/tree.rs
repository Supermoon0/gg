//! The HTML5 tree construction stage: insertion modes, the stack of
//! open elements, the list of active formatting elements, the adoption
//! agency algorithm, and foreign (SVG/MathML) content.
//!
//! Structured to mirror the spec's own division into insertion modes so
//! each `Mode` arm can be read against its section.

use std::collections::HashMap;

use super::scripts::Scripts;
use super::sink::{AttrName, Ns, NodeData, Quirks, Sink};
use super::tokenizer::{State as TState, Token, Tokenizer};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Initial,
    BeforeHtml,
    BeforeHead,
    InHead,
    InHeadNoscript,
    AfterHead,
    InBody,
    Text,
    InTable,
    InTableText,
    InCaption,
    InColumnGroup,
    InTableBody,
    InRow,
    InCell,
    InSelect,
    InSelectInTable,
    InTemplate,
    AfterBody,
    InFrameset,
    AfterFrameset,
    AfterAfterBody,
    AfterAfterFrameset,
}

/// An entry in the list of active formatting elements.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Formatting {
    Marker,
    Element(usize),
}

const SPECIAL: &[&str] = &[
    "address", "applet", "area", "article", "aside", "base", "basefont",
    "bgsound", "blockquote", "body", "br", "button", "caption", "center",
    "col", "colgroup", "dd", "details", "dir", "div", "dl", "dt", "embed",
    "fieldset", "figcaption", "figure", "footer", "form", "frame",
    "frameset", "h1", "h2", "h3", "h4", "h5", "h6", "head", "header",
    "hgroup", "hr", "html", "iframe", "img", "input", "keygen", "li",
    "link", "listing", "main", "marquee", "menu", "meta", "nav",
    "noembed", "noframes", "noscript", "object", "ol", "p", "param",
    "plaintext", "pre", "script", "search", "section", "select", "source",
    "style", "summary", "table", "tbody", "td", "template", "textarea",
    "tfoot", "th", "thead", "title", "tr", "track", "ul", "wbr", "xmp",
];

const FORMATTING: &[&str] = &[
    "a", "b", "big", "code", "em", "font", "i", "nobr", "s", "small",
    "strike", "strong", "tt", "u",
];

const IMPLIED_END: &[&str] =
    &["dd", "dt", "li", "optgroup", "option", "p", "rb", "rp", "rt", "rtc"];

pub struct TreeBuilder {
    pub sink: Sink,
    tok: Tokenizer,
    mode: Mode,
    original_mode: Mode,
    template_modes: Vec<Mode>,
    open: Vec<usize>,
    active: Vec<Formatting>,
    head: Option<usize>,
    form: Option<usize>,
    /// The element a fragment parse runs "inside", if any.
    context: Option<usize>,
    fragment: bool,
    scripting: bool,
    frameset_ok: bool,
    foster_parenting: bool,
    pending_table_text: Vec<char>,
    pending_table_text_ok: bool,
    ignore_lf: bool,
    done: bool,
    /// Every <selectedcontent> in the tree, so the mirror can be
    /// refreshed without walking the document.
    selected_content: Vec<usize>,
    /// Attributes an element was created with, for the formatting-list
    /// clones. Only formatting elements need an entry.
    token_attrs: HashMap<usize, Vec<(AttrName, String)>>,
    /// Present when scripting is enabled: scripts run as the parse
    /// reaches them.
    scripts: Option<Scripts>,
}

/// Result of a parse: the tree, plus (for fragment parsing) the root
/// whose children are the answer.
pub struct Parsed {
    pub sink: Sink,
    pub root: usize,
}

pub fn parse(input: &str, scripting: bool) -> Parsed {
    let mut tb = TreeBuilder::new(input, scripting, None);
    tb.run();
    let doc = tb.sink.document;
    Parsed { sink: tb.sink, root: doc }
}

/// Fragment parsing: `context` is "tagname" or "ns tagname"
/// (`svg path`, `math mi`), as html5lib-tests spells it.
pub fn parse_fragment(input: &str, context: &str, scripting: bool) -> Parsed {
    let (ns, name) = match context.split_once(' ') {
        Some(("svg", n)) => (Ns::Svg, n),
        Some(("math", n)) => (Ns::MathMl, n),
        _ => (Ns::Html, context),
    };
    let mut tb = TreeBuilder::new(input, scripting, Some((ns, name)));
    tb.run();
    let root = tb.open[0];
    Parsed { sink: tb.sink, root }
}

impl TreeBuilder {
    fn new(
        input: &str, scripting: bool, ctx: Option<(Ns, &str)>,
    ) -> TreeBuilder {
        let mut sink = Sink::new();
        let mut tb_open = Vec::new();
        let mut context = None;
        let mut fragment = false;
        let mut tok = Tokenizer::new(input);
        if let Some((ns, name)) = ctx {
            fragment = true;
            // A fragment parse runs against a synthetic <html> root
            // whose children are the result; the context element is
            // only consulted, never inserted.
            let html = sink.push(NodeData::Element {
                ns: Ns::Html,
                name: "html".to_string(),
                attrs: Vec::new(),
            });
            let doc = sink.document;
            sink.append(doc, html);
            tb_open.push(html);
            let ctx_node = sink.push(NodeData::Element {
                ns,
                name: name.to_string(),
                attrs: Vec::new(),
            });
            context = Some(ctx_node);
            // The context element decides the tokenizer's initial state.
            if ns == Ns::Html {
                tok.state = match name {
                    "title" | "textarea" => TState::Rcdata,
                    "style" | "xmp" | "iframe" | "noembed" | "noframes" => {
                        TState::Rawtext
                    }
                    "script" => TState::ScriptData,
                    "noscript" if scripting => TState::Rawtext,
                    "plaintext" => TState::Plaintext,
                    _ => TState::Data,
                };
            }
        }
        let mut tb = TreeBuilder {
            sink,
            tok,
            mode: if fragment { Mode::InBody } else { Mode::Initial },
            original_mode: Mode::InBody,
            template_modes: Vec::new(),
            open: tb_open,
            active: Vec::new(),
            head: None,
            form: None,
            context,
            fragment,
            scripting,
            frameset_ok: true,
            foster_parenting: false,
            pending_table_text: Vec::new(),
            pending_table_text_ok: true,
            ignore_lf: false,
            done: false,
            selected_content: Vec::new(),
            token_attrs: HashMap::new(),
            scripts: if scripting { Some(Scripts::new()) } else { None },
        };
        if fragment {
            if let Some(c) = tb.context {
                if tb.sink.is_html_element(c, "template") {
                    tb.template_modes.push(Mode::InTemplate);
                }
                tb.reset_insertion_mode();
                // form pointer: nearest form ancestor of the context
                if tb.sink.is_html_element(c, "form") {
                    tb.form = Some(c);
                }
            }
        }
        tb
    }

    fn run(&mut self) {
        while !self.done {
            self.tok.cdata_ok = self.cdata_allowed();
            let tokens = self.tok.next_tokens();
            if tokens.is_empty() {
                break;
            }
            for t in tokens {
                if self.done {
                    break;
                }
                self.process(t);
                if !self.selected_content.is_empty() {
                    self.refresh_selected_content();
                }
            }
        }
    }

    /// `<![CDATA[` is only real inside foreign content.
    fn cdata_allowed(&self) -> bool {
        match self.adjusted_current_node() {
            Some(n) => self.sink.ns(n) != Ns::Html,
            None => false,
        }
    }

    fn adjusted_current_node(&self) -> Option<usize> {
        if self.fragment && self.open.len() == 1 {
            self.context
        } else {
            self.open.last().copied()
        }
    }

    fn current(&self) -> usize {
        *self.open.last().unwrap()
    }

    fn cur_tag(&self) -> &str {
        match self.open.last() {
            Some(&n) => self.sink.tag(n),
            None => "",
        }
    }

    // ---- element insertion -------------------------------------------

    fn create(
        &mut self, ns: Ns, name: &str, attrs: Vec<(AttrName, String)>,
    ) -> usize {
        self.sink.push(NodeData::Element {
            ns,
            name: name.to_string(),
            attrs,
        })
    }

    /// The spec's "appropriate place for inserting a node", including
    /// foster parenting out of a table.
    fn insertion_place(&mut self) -> (usize, Option<usize>) {
        let target = self.current();
        if self.foster_parenting
            && matches!(
                self.sink.tag(target),
                "table" | "tbody" | "tfoot" | "thead" | "tr"
            )
            && self.sink.ns(target) == Ns::Html
        {
            // last template / last table, per spec
            let mut last_template = None;
            let mut last_table = None;
            for (i, &n) in self.open.iter().enumerate() {
                if self.sink.is_html_element(n, "template") {
                    last_template = Some((i, n));
                }
                if self.sink.is_html_element(n, "table") {
                    last_table = Some((i, n));
                }
            }
            match (last_template, last_table) {
                (Some((ti, t)), Some((tbi, _))) if ti > tbi => {
                    let c = self.sink.template_contents(t);
                    return (c, None);
                }
                (Some((_, t)), None) => {
                    let c = self.sink.template_contents(t);
                    return (c, None);
                }
                (_, Some((tbi, table))) => {
                    match self.sink.nodes[table].parent {
                        Some(p) => return (p, Some(table)),
                        None => {
                            // no parent: previous element on the stack
                            let prev = self.open[tbi.saturating_sub(1)];
                            return (prev, None);
                        }
                    }
                }
                (None, None) => {}
            }
        }
        if self.sink.is_html_element(target, "template") {
            let c = self.sink.template_contents(target);
            return (c, None);
        }
        (target, None)
    }

    fn insert_node(&mut self, node: usize) {
        let (parent, before) = self.insertion_place();
        match before {
            Some(b) => self.sink.insert_before(parent, node, b),
            None => self.sink.append(parent, node),
        }
    }

    fn insert_element(
        &mut self, ns: Ns, name: &str, attrs: Vec<(AttrName, String)>,
    ) -> usize {
        let n = self.create(ns, name, attrs);
        self.insert_node(n);
        // A template always owns a content fragment, even an empty one:
        // it is part of the element, not something the first insertion
        // brings into being.
        if ns == Ns::Html && name == "template" {
            self.sink.template_contents(n);
        }
        if ns == Ns::Html && name == "selectedcontent" {
            self.selected_content.push(n);
        }
        self.open.push(n);
        n
    }

    /// A `<selectedcontent>` displays a copy of its `<select>`'s selected
    /// option. The copy is rebuilt whenever the tree moves on, so it
    /// tracks both a later `selected` option and further content added
    /// inside the option already chosen.
    fn refresh_selected_content(&mut self) {
        for i in 0..self.selected_content.len() {
            let sc = self.selected_content[i];
            let Some(sel) = self.ancestor_select(sc) else { continue };
            let opt = self.selected_option(sel);
            for c in self.sink.nodes[sc].children.clone() {
                self.sink.detach(c);
            }
            let Some(opt) = opt else { continue };
            for c in self.sink.nodes[opt].children.clone() {
                let copy = self.sink.clone_deep(c);
                self.sink.append(sc, copy);
            }
        }
    }

    fn ancestor_select(&self, node: usize) -> Option<usize> {
        let mut n = node;
        while let Some(p) = self.sink.nodes[n].parent {
            if self.sink.is_html_element(p, "select") {
                return Some(p);
            }
            n = p;
        }
        None
    }

    /// The option a select displays: the last one explicitly `selected`,
    /// or — for a single-choice select — the first one that can be.
    fn selected_option(&self, sel: usize) -> Option<usize> {
        let mut first = None;
        let mut last_selected = None;
        let mut stack: Vec<usize> =
            self.sink.nodes[sel].children.iter().rev().copied().collect();
        while let Some(n) = stack.pop() {
            if self.sink.is_html_element(n, "select") {
                continue;
            }
            if self.sink.is_html_element(n, "option") {
                if self.sink.attr(n, "selected").is_some() {
                    last_selected = Some(n);
                }
                if first.is_none() && self.sink.attr(n, "disabled").is_none()
                {
                    first = Some(n);
                }
                continue;
            }
            stack.extend(self.sink.nodes[n].children.iter().rev().copied());
        }
        if last_selected.is_some() {
            return last_selected;
        }
        let multiple = self.sink.attr(sel, "multiple").is_some()
            || self
                .sink
                .attr(sel, "size")
                .and_then(|s| s.trim().parse::<u32>().ok())
                .is_some_and(|s| s > 1);
        if multiple {
            None
        } else {
            first
        }
    }

    fn insert_text(&mut self, text: &str) {
        let (parent, before) = self.insertion_place();
        match before {
            Some(b) => self.sink.insert_text_before(parent, b, text),
            None => self.sink.append_text(parent, text),
        }
    }

    fn insert_comment(&mut self, data: &str) {
        let n = self.sink.push(NodeData::Comment(data.to_string()));
        self.insert_node(n);
    }

    // ---- stack helpers -----------------------------------------------

    fn has_in_scope_list(&self, target: &[&str], list: &[(Ns, &str)]) -> bool {
        for &n in self.open.iter().rev() {
            let (ns, tag) = (self.sink.ns(n), self.sink.tag(n));
            if ns == Ns::Html && target.contains(&tag) {
                return true;
            }
            if list.iter().any(|&(l_ns, l)| l_ns == ns && l == tag) {
                return false;
            }
        }
        false
    }

    /// The elements a scope search stops at. <select> is one of them now
    /// that it holds arbitrary content: formatting outside a select must
    /// not reach in, so `<font><select></font>` leaves the <font> alone.
    fn scope_marks() -> &'static [(Ns, &'static str)] {
        &[
            (Ns::Html, "applet"),
            (Ns::Html, "caption"),
            (Ns::Html, "html"),
            (Ns::Html, "table"),
            (Ns::Html, "td"),
            (Ns::Html, "th"),
            (Ns::Html, "marquee"),
            (Ns::Html, "object"),
            (Ns::Html, "select"),
            (Ns::Html, "template"),
            (Ns::MathMl, "mi"),
            (Ns::MathMl, "mo"),
            (Ns::MathMl, "mn"),
            (Ns::MathMl, "ms"),
            (Ns::MathMl, "mtext"),
            (Ns::MathMl, "annotation-xml"),
            (Ns::Svg, "foreignObject"),
            (Ns::Svg, "desc"),
            (Ns::Svg, "title"),
        ]
    }

    fn in_scope(&self, tag: &str) -> bool {
        self.has_in_scope_list(&[tag], Self::scope_marks())
    }

    fn in_scope_any(&self, tags: &[&str]) -> bool {
        self.has_in_scope_list(tags, Self::scope_marks())
    }

    fn in_button_scope(&self, tag: &str) -> bool {
        let mut marks: Vec<(Ns, &str)> = Self::scope_marks().to_vec();
        marks.push((Ns::Html, "button"));
        self.has_in_scope_list(&[tag], &marks)
    }

    fn in_list_item_scope(&self, tag: &str) -> bool {
        let mut marks: Vec<(Ns, &str)> = Self::scope_marks().to_vec();
        marks.push((Ns::Html, "ol"));
        marks.push((Ns::Html, "ul"));
        self.has_in_scope_list(&[tag], &marks)
    }

    fn in_table_scope(&self, tags: &[&str]) -> bool {
        self.has_in_scope_list(
            tags,
            &[(Ns::Html, "html"), (Ns::Html, "table"), (Ns::Html, "template")],
        )
    }

    fn in_select_scope(&self, tag: &str) -> bool {
        for &n in self.open.iter().rev() {
            let t = self.sink.tag(n);
            if self.sink.ns(n) == Ns::Html && t == tag {
                return true;
            }
            if !(self.sink.ns(n) == Ns::Html
                && (t == "optgroup" || t == "option"))
            {
                return false;
            }
        }
        false
    }

    fn generate_implied_end(&mut self, except: Option<&str>) {
        while {
            let t = self.cur_tag();
            self.sink.ns(self.current()) == Ns::Html
                && IMPLIED_END.contains(&t)
                && Some(t) != except
        } {
            self.open.pop();
        }
    }

    fn generate_implied_end_thoroughly(&mut self) {
        const ALL: &[&str] = &[
            "caption", "colgroup", "dd", "dt", "li", "optgroup", "option",
            "p", "rb", "rp", "rt", "rtc", "tbody", "td", "tfoot", "th",
            "thead", "tr",
        ];
        while {
            let t = self.cur_tag();
            self.sink.ns(self.current()) == Ns::Html && ALL.contains(&t)
        } {
            self.open.pop();
        }
    }

    fn close_p(&mut self) {
        self.generate_implied_end(Some("p"));
        while let Some(&n) = self.open.last() {
            let is_p = self.sink.is_html_element(n, "p");
            self.open.pop();
            if is_p {
                break;
            }
        }
    }

    fn pop_until_html(&mut self, name: &str) {
        while let Some(&n) = self.open.last() {
            let hit = self.sink.is_html_element(n, name);
            self.open.pop();
            if hit {
                break;
            }
        }
    }

    fn pop_until_any(&mut self, names: &[&str]) {
        while let Some(&n) = self.open.last() {
            let hit = self.sink.ns(n) == Ns::Html
                && names.contains(&self.sink.tag(n));
            self.open.pop();
            if hit {
                break;
            }
        }
    }

    // ---- active formatting elements ----------------------------------

    fn push_active(&mut self, node: usize) {
        // The list clones the *token*, not the element: a script that
        // edits an element's attributes must not change the copies the
        // parser makes of it afterwards.
        let attrs = self.sink.attrs(node).to_vec();
        self.token_attrs.insert(node, attrs);
        // Noah's Ark: at most three identical entries after the last
        // marker.
        let mut count = 0;
        let mut remove: Option<usize> = None;
        for i in (0..self.active.len()).rev() {
            match self.active[i] {
                Formatting::Marker => break,
                Formatting::Element(e) => {
                    if self.same_formatting(e, node) {
                        count += 1;
                        if count == 3 {
                            remove = Some(i);
                            break;
                        }
                    }
                }
            }
        }
        if let Some(i) = remove {
            self.active.remove(i);
        }
        self.active.push(Formatting::Element(node));
    }

    /// The attributes an element was created with.
    fn token_attrs(&self, node: usize) -> Vec<(AttrName, String)> {
        match self.token_attrs.get(&node) {
            Some(a) => a.clone(),
            None => self.sink.attrs(node).to_vec(),
        }
    }

    fn same_formatting(&self, a: usize, b: usize) -> bool {
        if self.sink.tag(a) != self.sink.tag(b)
            || self.sink.ns(a) != self.sink.ns(b)
        {
            return false;
        }
        let (aa, ba) = (self.token_attrs(a), self.token_attrs(b));
        if aa.len() != ba.len() {
            return false;
        }
        aa.iter().all(|(k, v)| {
            ba.iter().any(|(k2, v2)| k == k2 && v == v2)
        })
    }

    fn push_marker(&mut self) {
        self.active.push(Formatting::Marker);
    }

    fn clear_active_to_marker(&mut self) {
        while let Some(f) = self.active.pop() {
            if f == Formatting::Marker {
                break;
            }
        }
    }

    fn reconstruct_active(&mut self) {
        let Some(&last) = self.active.last() else { return };
        let Formatting::Element(e) = last else { return };
        if self.open.contains(&e) {
            return;
        }
        // rewind
        let mut i = self.active.len() - 1;
        loop {
            if i == 0 {
                break;
            }
            i -= 1;
            match self.active[i] {
                Formatting::Marker => {
                    i += 1;
                    break;
                }
                Formatting::Element(e) if self.open.contains(&e) => {
                    i += 1;
                    break;
                }
                _ => {}
            }
        }
        // advance: recreate each entry
        while i < self.active.len() {
            let Formatting::Element(e) = self.active[i] else {
                i += 1;
                continue;
            };
            let (ns, name) = match &self.sink.nodes[e].data {
                NodeData::Element { ns, name, .. } => (*ns, name.clone()),
                _ => {
                    i += 1;
                    continue;
                }
            };
            let attrs = self.token_attrs(e);
            let n = self.insert_element(ns, &name, attrs.clone());
            self.token_attrs.insert(n, attrs);
            self.active[i] = Formatting::Element(n);
            i += 1;
        }
    }

    fn active_index(&self, node: usize) -> Option<usize> {
        self.active.iter().position(|f| *f == Formatting::Element(node))
    }

    /// The adoption agency algorithm — the spec's misnested-formatting
    /// repair, and the single most intricate part of tree construction.
    fn adoption_agency(&mut self, subject: &str) -> bool {
        // step 1: current node matches and is not in the active list
        let cur = self.current();
        if self.sink.ns(cur) == Ns::Html
            && self.sink.tag(cur) == subject
            && self.active_index(cur).is_none()
        {
            self.open.pop();
            return true;
        }
        let mut outer = 0;
        while outer < 8 {
            outer += 1;
            // formatting element: last entry after the last marker
            let mut fmt: Option<(usize, usize)> = None; // (active idx, node)
            for i in (0..self.active.len()).rev() {
                match self.active[i] {
                    Formatting::Marker => break,
                    Formatting::Element(e) => {
                        if self.sink.ns(e) == Ns::Html
                            && self.sink.tag(e) == subject
                        {
                            fmt = Some((i, e));
                            break;
                        }
                    }
                }
            }
            let Some((fmt_ai, fmt_node)) = fmt else {
                // "any other end tag" handling
                return false;
            };
            let Some(fmt_oi) = self.open.iter().position(|&n| n == fmt_node)
            else {
                self.active.remove(fmt_ai);
                return true;
            };
            if !self.in_scope(subject) {
                return true;
            }
            // furthest block: first special element below fmt on stack
            let furthest = self.open[fmt_oi + 1..].iter().copied().find(|&n| {
                self.sink.ns(n) == Ns::Html && SPECIAL.contains(&self.sink.tag(n))
            });
            let Some(furthest) = furthest else {
                self.open.truncate(fmt_oi);
                self.active.remove(fmt_ai);
                return true;
            };
            let common_ancestor = self.open[fmt_oi - 1];
            let mut bookmark = fmt_ai;
            let mut node_oi = self.open.iter().position(|&n| n == furthest).unwrap();
            let mut node;
            let mut last_node = furthest;
            let mut inner = 0;
            loop {
                inner += 1;
                if node_oi == 0 {
                    break;
                }
                node_oi -= 1;
                node = self.open[node_oi];
                if node == fmt_node {
                    break;
                }
                let ai = self.active_index(node);
                if inner > 3 {
                    if let Some(ai) = ai {
                        self.active.remove(ai);
                        if bookmark > ai {
                            bookmark -= 1;
                        }
                    }
                    self.open.remove(node_oi);
                    continue;
                }
                let Some(ai) = ai else {
                    self.open.remove(node_oi);
                    continue;
                };
                // replace node with a fresh copy
                let (ns, name) = match &self.sink.nodes[node].data {
                    NodeData::Element { ns, name, .. } => (*ns, name.clone()),
                    _ => unreachable!(),
                };
                let attrs = self.token_attrs(node);
                let fresh = self.create(ns, &name, attrs.clone());
                self.token_attrs.insert(fresh, attrs);
                self.active[ai] = Formatting::Element(fresh);
                self.open[node_oi] = fresh;
                node = fresh;
                if last_node == furthest {
                    bookmark = ai + 1;
                }
                self.sink.append(node, last_node);
                last_node = node;
            }
            // place last_node into the common ancestor
            let (parent, before) = self.foster_or(common_ancestor);
            match before {
                Some(b) => self.sink.insert_before(parent, last_node, b),
                None => self.sink.append(parent, last_node),
            }
            // a fresh copy of the formatting element takes the
            // furthest block's children
            let (ns, name) = match &self.sink.nodes[fmt_node].data {
                NodeData::Element { ns, name, .. } => (*ns, name.clone()),
                _ => unreachable!(),
            };
            let attrs = self.token_attrs(fmt_node);
            let fresh = self.create(ns, &name, attrs.clone());
            self.token_attrs.insert(fresh, attrs);
            let kids = self.sink.nodes[furthest].children.clone();
            for k in kids {
                self.sink.append(fresh, k);
            }
            self.sink.append(furthest, fresh);
            if let Some(ai) = self.active_index(fmt_node) {
                self.active.remove(ai);
                if bookmark > ai {
                    bookmark -= 1;
                }
            }
            bookmark = bookmark.min(self.active.len());
            self.active.insert(bookmark, Formatting::Element(fresh));
            if let Some(oi) = self.open.iter().position(|&n| n == fmt_node) {
                self.open.remove(oi);
            }
            let fi = self.open.iter().position(|&n| n == furthest).unwrap();
            self.open.insert(fi + 1, fresh);
        }
        true
    }

    /// Foster-parenting-aware placement for a specific target parent.
    fn foster_or(&mut self, target: usize) -> (usize, Option<usize>) {
        if self.foster_parenting
            && self.sink.ns(target) == Ns::Html
            && matches!(
                self.sink.tag(target),
                "table" | "tbody" | "tfoot" | "thead" | "tr"
            )
        {
            let save = self.open.clone();
            // reuse insertion_place by temporarily making target current
            if let Some(pos) = save.iter().position(|&n| n == target) {
                self.open.truncate(pos + 1);
                let place = self.insertion_place();
                self.open = save;
                return place;
            }
        }
        if self.sink.is_html_element(target, "template") {
            let c = self.sink.template_contents(target);
            return (c, None);
        }
        (target, None)
    }

    // ---- insertion-mode reset ----------------------------------------

    fn reset_insertion_mode(&mut self) {
        let mut last = false;
        for i in (0..self.open.len()).rev() {
            let mut node = self.open[i];
            if i == 0 {
                last = true;
                if self.fragment {
                    if let Some(c) = self.context {
                        node = c;
                    }
                }
            }
            let tag = self.sink.tag(node).to_string();
            if self.sink.ns(node) != Ns::Html {
                if !last {
                    continue;
                }
            }
            match tag.as_str() {
                "select" => {
                    if !last {
                        for j in (0..i).rev() {
                            let a = self.open[j];
                            if self.sink.is_html_element(a, "template") {
                                break;
                            }
                            if self.sink.is_html_element(a, "table") {
                                self.mode = Mode::InSelectInTable;
                                return;
                            }
                        }
                    }
                    self.mode = Mode::InSelect;
                    return;
                }
                "td" | "th" if !last => {
                    self.mode = Mode::InCell;
                    return;
                }
                "tr" => {
                    self.mode = Mode::InRow;
                    return;
                }
                "tbody" | "thead" | "tfoot" => {
                    self.mode = Mode::InTableBody;
                    return;
                }
                "caption" => {
                    self.mode = Mode::InCaption;
                    return;
                }
                "colgroup" => {
                    self.mode = Mode::InColumnGroup;
                    return;
                }
                "table" => {
                    self.mode = Mode::InTable;
                    return;
                }
                "template" => {
                    self.mode =
                        *self.template_modes.last().unwrap_or(&Mode::InBody);
                    return;
                }
                "head" if !last => {
                    self.mode = Mode::InHead;
                    return;
                }
                "body" => {
                    self.mode = Mode::InBody;
                    return;
                }
                "frameset" => {
                    self.mode = Mode::InFrameset;
                    return;
                }
                "html" => {
                    self.mode = if self.head.is_none() {
                        Mode::BeforeHead
                    } else {
                        Mode::AfterHead
                    };
                    return;
                }
                _ => {}
            }
            if last {
                self.mode = Mode::InBody;
                return;
            }
        }
        self.mode = Mode::InBody;
    }
}

fn is_ws(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\u{0c}' | '\r' | ' ')
}

include!("tree_modes.rs");
include!("tree_tables.rs");
