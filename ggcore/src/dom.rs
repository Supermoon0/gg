//! Flat-arena DOM: nodes live in one Vec, linked by indices.

use std::collections::{HashMap, HashSet};

/// What a node *is*. The engine used to infer this from `tag`: an
/// element had one, everything else was text. A parser written to the
/// spec produces comments, a doctype and template content too, so the
/// distinction has to be carried rather than guessed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeKind {
    Element,
    Text,
    Comment,
    /// `<?target data>` — the spec grew tokenizer states for these.
    ProcessingInstruction { target: String },
    /// A `<template>`'s content: in the tree, but not in the document.
    DocumentFragment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Namespace {
    Html,
    Svg,
    MathMl,
}

impl Namespace {
    pub fn as_url(self) -> &'static str {
        match self {
            Namespace::Html => "http://www.w3.org/1999/xhtml",
            Namespace::Svg => "http://www.w3.org/2000/svg",
            Namespace::MathMl => "http://www.w3.org/1998/Math/MathML",
        }
    }

    pub fn from_url(url: &str) -> Namespace {
        match url {
            "http://www.w3.org/2000/svg" => Namespace::Svg,
            "http://www.w3.org/1998/Math/MathML" => Namespace::MathMl,
            _ => Namespace::Html,
        }
    }

    /// How html5lib-tests prefixes a non-HTML element.
    pub fn prefix(self) -> &'static str {
        match self {
            Namespace::Html => "",
            Namespace::Svg => "svg ",
            Namespace::MathMl => "math ",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Doctype {
    pub name: String,
    pub public_id: String,
    pub system_id: String,
}

/// A child of the Document node. The doctype is metadata rather than a
/// node in this arena, but its position among the comments around the
/// root element is observable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DocumentChild {
    Doctype,
    Node(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quirks {
    NoQuirks,
    LimitedQuirks,
    Quirks,
}

pub struct Node {
    pub parent: Option<usize>,
    pub kind: NodeKind,
    /// Present for element nodes only.
    pub tag: Option<String>,
    pub namespace: Namespace,
    pub text: String,
    pub attrs: Vec<(String, String)>,
    /// Namespace prefix and local name per attribute, aligned with
    /// `attrs`. `None` is an ordinary unprefixed HTML attribute; the
    /// pair is what html5lib prints, e.g. `("xlink", "href")`.
    pub attr_namespaces: Vec<Option<(String, String)>>,
    pub children: Vec<usize>,
    pub style: HashMap<String, String>,
    /// cached class list (filled lazily during style computation)
    pub classes: Vec<String>,
}

impl Node {
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn is_element(&self) -> bool {
        self.kind == NodeKind::Element
    }

    pub fn is_text(&self) -> bool {
        self.kind == NodeKind::Text
    }

    /// Whether layout and painting should see this node at all.
    pub fn is_renderable(&self) -> bool {
        matches!(self.kind, NodeKind::Element | NodeKind::Text)
    }
}

pub struct Document {
    pub nodes: Vec<Node>,
    pub root: usize,
    /// The document type, if the page declared one.
    pub doctype: Option<Doctype>,
    /// Children of the Document node — the root element plus whatever
    /// comments and the doctype sit around it.
    pub document_children: Vec<DocumentChild>,
    /// <template> element -> its detached content fragment.
    pub template_contents: HashMap<usize, usize>,
    /// Decided from the doctype; the style layer will want it.
    pub quirks: Quirks,
    /// id -> candidate node indices in creation order, so
    /// getElementById is O(1) instead of an arena scan.
    pub id_map: HashMap<String, Vec<usize>>,
    /// bumped on every structural/attribute mutation — the render
    /// loop re-styles/re-lays-out only when this changes
    pub version: u64,
    /// :hover state — the element under the pointer and its ancestors
    /// (the shell sets this before a hover restyle)
    pub hover_chain: Vec<usize>,
    /// :focus state — the focused element, if any
    pub focused: Option<usize>,
    /// Explicit HTMLScriptElement.async property writes. A dynamically
    /// created script defaults to async, so `script.async = false` must be
    /// distinguishable from an untouched element with no async attribute.
    pub script_async_overrides: HashMap<usize, bool>,
    /// Script elements created through the DOM API. Unlike parser-created
    /// scripts, their force-async flag defaults to true.
    pub script_created_dynamically: HashSet<usize>,
}

impl Document {
    pub fn with_capacity(cap: usize) -> Document {
        Document {
            nodes: Vec::with_capacity(cap),
            root: 0,
            doctype: None,
            document_children: Vec::new(),
            template_contents: HashMap::new(),
            quirks: Quirks::NoQuirks,
            id_map: HashMap::new(),
            version: 0,
            hover_chain: Vec::new(),
            focused: None,
            script_async_overrides: HashMap::new(),
            script_created_dynamically: HashSet::new(),
        }
    }

    /// First connected element with this id (creation order approximates
    /// document order; duplicate ids are a page bug anyway).
    pub fn get_element_by_id(&self, id: &str) -> Option<usize> {
        let cands = self.id_map.get(id)?;
        cands.iter().copied().find(|&i| self.is_connected(i))
    }

    pub fn is_connected(&self, mut idx: usize) -> bool {
        loop {
            if idx == self.root {
                return true;
            }
            match self.nodes[idx].parent {
                Some(p) => idx = p,
                None => return false,
            }
        }
    }

    pub fn new_element(
        &mut self,
        tag: String,
        attrs: Vec<(String, String)>,
        parent: Option<usize>,
    ) -> usize {
        self.new_element_ns(tag, attrs, parent, Namespace::Html)
    }

    pub fn new_element_ns(
        &mut self,
        tag: String,
        attrs: Vec<(String, String)>,
        parent: Option<usize>,
        namespace: Namespace,
    ) -> usize {
        self.version += 1;
        let attr_count = attrs.len();
        let classes = attrs
            .iter()
            .find(|(k, _)| k == "class")
            .map(|(_, v)| v.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default();
        let idx = self.nodes.len();
        self.nodes.push(Node {
            parent,
            kind: NodeKind::Element,
            tag: Some(tag),
            namespace,
            text: String::new(),
            attrs,
            attr_namespaces: vec![None; attr_count],
            children: Vec::new(),
            style: HashMap::new(),
            classes,
        });
        if let Some(p) = parent {
            self.nodes[p].children.push(idx);
        }
        if let Some(id) = self.nodes[idx].attr("id") {
            let id = id.to_string();
            self.id_map.entry(id).or_default().push(idx);
        }
        idx
    }

    pub fn set_attr(&mut self, idx: usize, name: &str, value: &str) {
        self.version += 1;
        if name == "id" {
            if let Some(old) = self.nodes[idx].attr("id") {
                let old = old.to_string();
                if let Some(v) = self.id_map.get_mut(&old) {
                    v.retain(|&i| i != idx);
                }
            }
            self.id_map.entry(value.to_string()).or_default().push(idx);
        }
        let node = &mut self.nodes[idx];
        if let Some(pair) = node.attrs.iter_mut().find(|(k, _)| k == name) {
            pair.1 = value.to_string();
        } else {
            node.attrs.push((name.to_string(), value.to_string()));
            node.attr_namespaces.push(None);
        }
        if name == "class" {
            node.classes =
                value.split_whitespace().map(str::to_string).collect();
        }
    }

    pub fn remove_attr(&mut self, idx: usize, name: &str) {
        self.version += 1;
        if name == "id" {
            if let Some(old) = self.nodes[idx].attr("id") {
                let old = old.to_string();
                if let Some(v) = self.id_map.get_mut(&old) {
                    v.retain(|&i| i != idx);
                }
            }
        }
        let node = &mut self.nodes[idx];
        if let Some(at) = node.attrs.iter().position(|(k, _)| k == name) {
            node.attrs.remove(at);
            if at < node.attr_namespaces.len() {
                node.attr_namespaces.remove(at);
            }
        }
        if name == "class" {
            node.classes.clear();
        }
    }

    /// Concatenated descendant text (like textContent).
    pub fn collect_text(&self, idx: usize) -> String {
        let mut out = String::new();
        let mut stack = vec![idx];
        while let Some(i) = stack.pop() {
            let node = &self.nodes[i];
            if node.is_text() {
                out.push_str(&node.text);
            }
            for &c in node.children.iter().rev() {
                stack.push(c);
            }
        }
        out
    }

    /// html5lib/WPT's canonical tree dump, taken from the engine's own
    /// DOM. Nothing else in the engine reads comments, the doctype or a
    /// template's content; printing them from here is what proves the
    /// parser's output crosses into this tree intact.
    pub fn html5lib_tree_dump(&self) -> String {
        let mut out = String::new();
        for child in &self.document_children {
            match child {
                DocumentChild::Doctype => {
                    let Some(d) = &self.doctype else { continue };
                    out.push_str(&format!("| <!DOCTYPE {}", d.name));
                    if !d.public_id.is_empty() || !d.system_id.is_empty() {
                        out.push_str(&format!(
                            " \"{}\" \"{}\"", d.public_id, d.system_id));
                    }
                    out.push_str(">\n");
                }
                DocumentChild::Node(idx) => self.dump_node(*idx, 0, &mut out),
            }
        }
        out
    }

    /// As `html5lib_tree_dump`, but starting inside a node — the shape a
    /// fragment parse is compared in.
    pub fn html5lib_children_dump(&self, node: usize) -> String {
        let mut out = String::new();
        for &c in &self.nodes[node].children {
            self.dump_node(c, 0, &mut out);
        }
        out
    }

    fn dump_node(&self, idx: usize, depth: usize, out: &mut String) {
        let pad = "  ".repeat(depth);
        let node = &self.nodes[idx];
        match &node.kind {
            NodeKind::Element => {
                out.push_str(&format!(
                    "| {pad}<{}{}>\n",
                    node.namespace.prefix(),
                    node.tag.as_deref().unwrap_or(""),
                ));
                let mut sorted: Vec<(String, &String)> = node
                    .attrs
                    .iter()
                    .enumerate()
                    .map(|(i, (k, v))| {
                        let name = node
                            .attr_namespaces
                            .get(i)
                            .and_then(Option::as_ref)
                            .map_or_else(
                                || k.clone(),
                                |(prefix, local)| {
                                    format!("{prefix} {local}")
                                },
                            );
                        (name, v)
                    })
                    .collect();
                sorted.sort_by(|a, b| a.0.cmp(&b.0));
                let apad = "  ".repeat(depth + 1);
                for (k, v) in sorted {
                    out.push_str(&format!("| {apad}{k}=\"{v}\"\n"));
                }
                if let Some(&fragment) = self.template_contents.get(&idx) {
                    out.push_str(&format!("| {apad}content\n"));
                    for &c in &self.nodes[fragment].children {
                        self.dump_node(c, depth + 2, out);
                    }
                }
                for &c in &node.children {
                    self.dump_node(c, depth + 1, out);
                }
            }
            NodeKind::Text => {
                out.push_str(&format!("| {pad}\"{}\"\n", node.text));
            }
            NodeKind::Comment => {
                out.push_str(&format!("| {pad}<!-- {} -->\n", node.text));
            }
            NodeKind::ProcessingInstruction { target } => {
                out.push_str(&format!(
                    "| {pad}<?{} {}?>\n", target, node.text));
            }
            NodeKind::DocumentFragment => {
                for &c in &node.children {
                    self.dump_node(c, depth, out);
                }
            }
        }
    }

    /// Detach a node from its parent (if any).
    pub fn detach(&mut self, idx: usize) {
        self.version += 1;
        if let Some(p) = self.nodes[idx].parent {
            self.nodes[p].children.retain(|&c| c != idx);
            self.nodes[idx].parent = None;
        }
    }

    /// Deep-copy a node (and subtree) from another document into self.
    pub fn graft(&mut self, other: &Document, src: usize, parent: usize) {
        self.version += 1;
        // Iterative: markup that arrives through document.write can be
        // arbitrarily deep, and the recursion used to be the stack's
        // problem rather than the caller's.
        let mut stack = vec![(src, parent)];
        while let Some((src, parent)) = stack.pop() {
            let node = &other.nodes[src];
            let new_idx = match &node.kind {
                NodeKind::Element => {
                    let n = self.new_element_ns(
                        node.tag.clone().unwrap_or_default(),
                        node.attrs.clone(),
                        Some(parent),
                        node.namespace,
                    );
                    self.nodes[n].attr_namespaces =
                        node.attr_namespaces.clone();
                    n
                }
                NodeKind::Text => self.new_text(node.text.clone(), parent),
                NodeKind::Comment => {
                    self.new_comment(node.text.clone(), Some(parent))
                }
                NodeKind::ProcessingInstruction { target } => self.new_pi(
                    target.clone(), node.text.clone(), Some(parent)),
                NodeKind::DocumentFragment => continue,
            };
            for &c in other.nodes[src].children.iter().rev() {
                stack.push((c, new_idx));
            }
        }
    }

    /// `el.textContent = s`: drop the subtree, leave one text node.
    pub fn set_text_content(&mut self, idx: usize, text: &str) {
        self.version += 1;
        for child in std::mem::take(&mut self.nodes[idx].children) {
            self.nodes[child].parent = None;
        }
        if !text.is_empty() {
            self.new_text(text.to_string(), idx);
        }
    }

    pub fn new_text(&mut self, text: String, parent: usize) -> usize {
        self.new_node(NodeKind::Text, text, Some(parent))
    }

    pub fn new_comment(
        &mut self, text: String, parent: Option<usize>,
    ) -> usize {
        self.new_node(NodeKind::Comment, text, parent)
    }

    pub fn new_pi(
        &mut self, target: String, data: String, parent: Option<usize>,
    ) -> usize {
        self.new_node(
            NodeKind::ProcessingInstruction { target }, data, parent)
    }

    /// A template's content lives in a fragment with no parent.
    pub fn new_fragment(&mut self) -> usize {
        self.new_node(NodeKind::DocumentFragment, String::new(), None)
    }

    fn new_node(
        &mut self, kind: NodeKind, text: String, parent: Option<usize>,
    ) -> usize {
        self.version += 1;
        let idx = self.nodes.len();
        self.nodes.push(Node {
            parent,
            kind,
            tag: None,
            namespace: Namespace::Html,
            text,
            attrs: Vec::new(),
            attr_namespaces: Vec::new(),
            children: Vec::new(),
            style: HashMap::new(),
            classes: Vec::new(),
        });
        if let Some(parent) = parent {
            self.nodes[parent].children.push(idx);
        }
        idx
    }
}
