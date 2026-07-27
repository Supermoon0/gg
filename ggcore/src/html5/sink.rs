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

/// Adapter into the engine's DOM. Comments and the doctype are dropped
/// (the engine has never modelled them); everything else transfers,
/// with namespaced names flattened the way the rest of the engine
/// expects to read them.
pub fn to_dom(sink: &Sink) -> crate::dom::Document {
    let mut doc = crate::dom::Document::with_capacity(sink.nodes.len().max(16));
    // The engine's Document is rooted at <html>, not at a document node.
    let root = sink.nodes[sink.document]
        .children
        .iter()
        .copied()
        .find(|&c| sink.is_html_element(c, "html"));
    let Some(root) = root else {
        let idx = doc.new_element("html".to_string(), Vec::new(), None);
        doc.root = idx;
        return doc;
    };
    let mut map: HashMap<usize, usize> = HashMap::new();
    let idx = doc.new_element("html".to_string(), dom_attrs(sink, root), None);
    doc.root = idx;
    map.insert(root, idx);
    let mut stack = vec![root];
    while let Some(src) = stack.pop() {
        let parent = map[&src];
        for &c in &sink.nodes[src].children {
            match &sink.nodes[c].data {
                NodeData::Element { name, .. } => {
                    let n = doc.new_element(
                        name.clone(), dom_attrs(sink, c), Some(parent));
                    map.insert(c, n);
                    stack.push(c);
                }
                NodeData::Text(t) => {
                    doc.new_text(t.clone(), parent);
                }
                // template contents are not part of the rendered tree
                _ => {}
            }
        }
    }
    doc
}

fn dom_attrs(sink: &Sink, idx: usize) -> Vec<(String, String)> {
    sink.attrs(idx)
        .iter()
        .map(|(k, v)| (k.serialized().replace(' ', ":"), v.clone()))
        .collect()
}
