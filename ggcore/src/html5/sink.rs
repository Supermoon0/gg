//! The tree an HTML5 parse produces.
//!
//! Deliberately separate from `crate::dom::Document`: the spec tree
//! needs things the engine's DOM has never carried (comments, a
//! doctype, element namespaces, namespaced attributes, a template's
//! separate content fragment). Keeping them apart lets the parser be
//! measured against html5lib-tests on its own terms, and adapted into
//! the engine's DOM at the boundary.

use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ns {
    Html,
    Svg,
    MathMl,
}

impl Ns {
    /// html5lib-tests prints a namespace prefix for non-HTML elements.
    pub fn prefix(self) -> &'static str {
        match self {
            Ns::Html => "",
            Ns::Svg => "svg ",
            Ns::MathMl => "math ",
        }
    }
}

/// An attribute name: an optional namespace prefix plus a local name.
/// `xlink:href` on an SVG element becomes prefix "xlink", local "href",
/// which html5lib prints as `xlink href="..."`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AttrName {
    pub prefix: Option<String>,
    pub local: String,
}

impl AttrName {
    pub fn local(name: impl Into<String>) -> AttrName {
        AttrName { prefix: None, local: name.into() }
    }

    /// How html5lib-tests spells it, which is also the sort key.
    pub fn serialized(&self) -> String {
        match &self.prefix {
            Some(p) => format!("{p} {}", self.local),
            None => self.local.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub enum NodeData {
    Document,
    Doctype { name: String, public_id: String, system_id: String },
    Element { ns: Ns, name: String, attrs: Vec<(AttrName, String)> },
    Text(String),
    Comment(String),
    /// A `<template>`'s contents live in a separate fragment; the spec
    /// treats it as a distinct document fragment hanging off the
    /// element, and html5lib prints it under a `content` line.
    TemplateContents,
}

pub struct Node {
    pub data: NodeData,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
}

pub struct Sink {
    pub nodes: Vec<Node>,
    pub document: usize,
    /// Quirks mode is decided from the doctype and read back by tests
    /// and (later) by the engine's style layer.
    pub quirks: Quirks,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Quirks {
    NoQuirks,
    LimitedQuirks,
    Quirks,
}

impl Sink {
    pub fn new() -> Sink {
        Sink {
            nodes: vec![Node {
                data: NodeData::Document,
                parent: None,
                children: Vec::new(),
            }],
            document: 0,
            quirks: Quirks::NoQuirks,
        }
    }

    pub fn push(&mut self, data: NodeData) -> usize {
        let idx = self.nodes.len();
        self.nodes.push(Node { data, parent: None, children: Vec::new() });
        idx
    }

    pub fn append(&mut self, parent: usize, child: usize) {
        self.detach(child);
        self.nodes[child].parent = Some(parent);
        self.nodes[parent].children.push(child);
    }

    pub fn insert_before(&mut self, parent: usize, child: usize, before: usize) {
        self.detach(child);
        let at = self.nodes[parent]
            .children
            .iter()
            .position(|&c| c == before)
            .unwrap_or(self.nodes[parent].children.len());
        self.nodes[child].parent = Some(parent);
        self.nodes[parent].children.insert(at, child);
    }

    pub fn detach(&mut self, child: usize) {
        if let Some(p) = self.nodes[child].parent.take() {
            self.nodes[p].children.retain(|&c| c != child);
        }
    }

    pub fn tag(&self, idx: usize) -> &str {
        match &self.nodes[idx].data {
            NodeData::Element { name, .. } => name,
            _ => "",
        }
    }

    pub fn ns(&self, idx: usize) -> Ns {
        match &self.nodes[idx].data {
            NodeData::Element { ns, .. } => *ns,
            _ => Ns::Html,
        }
    }

    pub fn is_html_element(&self, idx: usize, name: &str) -> bool {
        matches!(&self.nodes[idx].data,
                 NodeData::Element { ns: Ns::Html, name: n, .. } if n == name)
    }

    pub fn attrs(&self, idx: usize) -> &[(AttrName, String)] {
        match &self.nodes[idx].data {
            NodeData::Element { attrs, .. } => attrs,
            _ => &[],
        }
    }

    pub fn attr(&self, idx: usize, name: &str) -> Option<&str> {
        self.attrs(idx)
            .iter()
            .find(|(k, _)| k.prefix.is_none() && k.local == name)
            .map(|(_, v)| v.as_str())
    }

    /// The spec inserts one character at a time; building a String for
    /// each one costs more than the insertion does.
    pub fn append_char(&mut self, parent: usize, c: char) {
        if let Some(&last) = self.nodes[parent].children.last() {
            if let NodeData::Text(t) = &mut self.nodes[last].data {
                t.push(c);
                return;
            }
        }
        let n = self.push(NodeData::Text(c.to_string()));
        self.append(parent, n);
    }

    /// Append text, merging into a preceding text sibling as the spec's
    /// "insert a character" step does.
    pub fn append_text(&mut self, parent: usize, text: &str) {
        if let Some(&last) = self.nodes[parent].children.last() {
            if let NodeData::Text(t) = &mut self.nodes[last].data {
                t.push_str(text);
                return;
            }
        }
        let n = self.push(NodeData::Text(text.to_string()));
        self.append(parent, n);
    }

    pub fn insert_char_before(
        &mut self, parent: usize, before: usize, c: char,
    ) {
        let at = self.nodes[parent]
            .children
            .iter()
            .position(|&x| x == before)
            .unwrap_or(self.nodes[parent].children.len());
        if at > 0 {
            let prev = self.nodes[parent].children[at - 1];
            if let NodeData::Text(t) = &mut self.nodes[prev].data {
                t.push(c);
                return;
            }
        }
        let n = self.push(NodeData::Text(c.to_string()));
        self.insert_before(parent, n, before);
    }

    pub fn insert_text_before(
        &mut self, parent: usize, before: usize, text: &str,
    ) {
        let at = self.nodes[parent]
            .children
            .iter()
            .position(|&c| c == before)
            .unwrap_or(self.nodes[parent].children.len());
        if at > 0 {
            let prev = self.nodes[parent].children[at - 1];
            if let NodeData::Text(t) = &mut self.nodes[prev].data {
                t.push_str(text);
                return;
            }
        }
        let n = self.push(NodeData::Text(text.to_string()));
        self.insert_before(parent, n, before);
    }

    /// A parentless deep copy of `node`. <selectedcontent> shows one of
    /// the selected option's children.
    pub fn clone_deep(&mut self, node: usize) -> usize {
        let copy = self.push(self.nodes[node].data.clone());
        for i in 0..self.nodes[node].children.len() {
            let child = self.nodes[node].children[i];
            let c = self.clone_deep(child);
            self.append(copy, c);
        }
        copy
    }

    /// The `content` fragment of a template element, created on demand.
    pub fn template_contents(&mut self, template: usize) -> usize {
        for &c in &self.nodes[template].children {
            if matches!(self.nodes[c].data, NodeData::TemplateContents) {
                return c;
            }
        }
        let c = self.push(NodeData::TemplateContents);
        self.append(template, c);
        c
    }

    /// html5lib-tests serialization: one line per node, `| ` then two
    /// spaces per level of depth.
    pub fn serialize(&self) -> String {
        let mut out = String::new();
        for &c in &self.nodes[self.document].children {
            self.serialize_node(c, 0, &mut out);
        }
        out
    }

    /// As `serialize`, but starting inside a node — the shape a
    /// fragment parse is compared in.
    pub fn serialize_children(&self, node: usize) -> String {
        let mut out = String::new();
        for &c in &self.nodes[node].children {
            self.serialize_node(c, 0, &mut out);
        }
        out
    }

    fn serialize_node(&self, idx: usize, depth: usize, out: &mut String) {
        let pad = "  ".repeat(depth);
        match &self.nodes[idx].data {
            NodeData::Document => {}
            NodeData::Doctype { name, public_id, system_id } => {
                out.push_str(&format!("| {pad}<!DOCTYPE {name}"));
                if !public_id.is_empty() || !system_id.is_empty() {
                    out.push_str(&format!(" \"{public_id}\" \"{system_id}\""));
                }
                out.push_str(">\n");
            }
            NodeData::Element { ns, name, attrs } => {
                out.push_str(&format!("| {pad}<{}{name}>\n", ns.prefix()));
                let mut sorted: Vec<_> = attrs
                    .iter()
                    .map(|(k, v)| (k.serialized(), v.clone()))
                    .collect();
                sorted.sort_by(|a, b| a.0.cmp(&b.0));
                let apad = "  ".repeat(depth + 1);
                for (k, v) in sorted {
                    out.push_str(&format!("| {apad}{k}=\"{v}\"\n"));
                }
                for &c in &self.nodes[idx].children {
                    self.serialize_node(c, depth + 1, out);
                }
            }
            NodeData::Text(t) => {
                out.push_str(&format!("| {pad}\"{t}\"\n"));
            }
            NodeData::Comment(t) => {
                out.push_str(&format!("| {pad}<!-- {t} -->\n"));
            }
            NodeData::TemplateContents => {
                out.push_str(&format!("| {pad}content\n"));
                for &c in &self.nodes[idx].children {
                    self.serialize_node(c, depth + 1, out);
                }
            }
        }
    }
}

/// Adapter into the engine's DOM. Nothing is dropped any more: the
/// engine's `Document` carries node kinds, namespaces, a doctype and a
/// template's content fragment, so a parse survives the crossing whole.
pub fn to_dom(sink: &Sink) -> crate::dom::Document {
    use crate::dom::{DocumentChild, Doctype};
    let mut doc = crate::dom::Document::with_capacity(sink.nodes.len() + 8);
    doc.quirks = match sink.quirks {
        Quirks::NoQuirks => crate::dom::Quirks::NoQuirks,
        Quirks::LimitedQuirks => crate::dom::Quirks::LimitedQuirks,
        Quirks::Quirks => crate::dom::Quirks::Quirks,
    };
    for &c in &sink.nodes[sink.document].children {
        match &sink.nodes[c].data {
            NodeData::Doctype { name, public_id, system_id } => {
                doc.doctype = Some(Doctype {
                    name: name.clone(),
                    public_id: public_id.clone(),
                    system_id: system_id.clone(),
                });
                doc.document_children.push(DocumentChild::Doctype);
            }
            NodeData::Element { .. } => {
                let idx = transfer(sink, c, None, &mut doc);
                if sink.is_html_element(c, "html") {
                    doc.root = idx;
                }
                doc.document_children.push(DocumentChild::Node(idx));
            }
            _ => {
                let idx = transfer(sink, c, None, &mut doc);
                doc.document_children.push(DocumentChild::Node(idx));
            }
        }
    }
    if doc.nodes.is_empty() {
        doc.root = doc.new_element("html".to_string(), Vec::new(), None);
    }
    doc
}

/// A fragment parse compares the context element's children, so the
/// engine's tree is rooted at a fragment rather than at <html>.
pub fn to_dom_fragment(sink: &Sink, root: usize) -> crate::dom::Document {
    let mut doc = crate::dom::Document::with_capacity(sink.nodes.len() + 8);
    let fragment = doc.new_fragment();
    doc.root = fragment;
    doc.document_children.push(crate::dom::DocumentChild::Node(fragment));
    for &c in &sink.nodes[root].children {
        transfer(sink, c, Some(fragment), &mut doc);
    }
    doc
}

/// Copy one parse node and its subtree across. Iterative: a document
/// can nest as deeply as its author cared to.
fn transfer(
    sink: &Sink, src: usize, parent: Option<usize>,
    doc: &mut crate::dom::Document,
) -> usize {
    let root = transfer_one(sink, src, parent, doc);
    let mut stack = vec![(src, root)];
    while let Some((s, d)) = stack.pop() {
        // Children are created in document order — a node is appended
        // the moment it is made — and only then queued in reverse, so
        // the stack walks them forwards.
        let mut made = Vec::with_capacity(sink.nodes[s].children.len());
        for &c in &sink.nodes[s].children {
            if matches!(sink.nodes[c].data, NodeData::TemplateContents) {
                // A template's content is its own fragment, not a child.
                let fragment = doc.new_fragment();
                doc.template_contents.insert(d, fragment);
                for &g in &sink.nodes[c].children {
                    let n = transfer_one(sink, g, Some(fragment), doc);
                    made.push((g, n));
                }
                continue;
            }
            let n = transfer_one(sink, c, Some(d), doc);
            made.push((c, n));
        }
        stack.extend(made.into_iter().rev());
    }
    root
}

fn transfer_one(
    sink: &Sink, src: usize, parent: Option<usize>,
    doc: &mut crate::dom::Document,
) -> usize {
    match &sink.nodes[src].data {
        NodeData::Element { ns, name, attrs } => {
            let ns = match ns {
                Ns::Html => crate::dom::Namespace::Html,
                Ns::Svg => crate::dom::Namespace::Svg,
                Ns::MathMl => crate::dom::Namespace::MathMl,
            };
            let flat: Vec<(String, String)> = attrs
                .iter()
                .map(|(k, v)| (k.serialized().replace(' ', ":"), v.clone()))
                .collect();
            let spaces: Vec<Option<(String, String)>> = attrs
                .iter()
                .map(|(k, _)| {
                    k.prefix.clone().map(|p| (p, k.local.clone()))
                })
                .collect();
            let idx = doc.new_element_ns(name.clone(), flat, parent, ns);
            doc.nodes[idx].attr_namespaces = spaces;
            idx
        }
        NodeData::Text(t) => match parent {
            Some(p) => doc.new_text(t.clone(), p),
            None => doc.new_text(t.clone(), 0),
        },
        NodeData::Comment(t) => doc.new_comment(t.clone(), parent),
        _ => doc.new_fragment(),
    }
}
