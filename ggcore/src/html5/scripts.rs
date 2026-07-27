//! Running `<script>` while the document is still being parsed.
//!
//! The spec puts script execution *inside* tree construction: a script
//! sees the tree built so far, and whatever it writes with
//! `document.write` is tokenized at the insertion point, ahead of the
//! rest of the input. Both are observable in the finished document, so a
//! parser that defers scripts to the end of the parse builds a different
//! tree.
//!
//! gg-js works on `crate::dom::Document`, not on the parser's own tree,
//! so this keeps one mirrored in the other and reconciles across every
//! script run: the tree goes down before the script, the script's edits
//! come back up after. The mirror is incremental — node identity is
//! stable, so a reference a script stashes in a global still resolves on
//! the next script.
//!
//! One thing does not survive the round trip: the engine's DOM has no
//! comment or doctype nodes, so if a script reorders a subtree that
//! contains them, they are dropped from that subtree. Nothing else about
//! them is affected, and no page has ever depended on it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use super::sink::{AttrName, NodeData, Ns, Sink};
use crate::dom;
use crate::jsvm::page::PageVm;

pub struct Scripts {
    vm: PageVm,
    doc: Rc<RefCell<dom::Document>>,
    s2d: HashMap<usize, usize>,
    d2s: HashMap<usize, usize>,
}

impl Scripts {
    pub fn new() -> Scripts {
        let doc = Rc::new(RefCell::new(dom::Document::with_capacity(64)));
        let mut vm = PageVm::new(Some(doc.clone()));
        vm.set_parser_writes(true);
        Scripts { vm, doc, s2d: HashMap::new(), d2s: HashMap::new() }
    }

    /// Run one script against the tree as it stands. Returns what
    /// `document.write` produced, for the caller to re-tokenize at the
    /// insertion point.
    pub fn run(
        &mut self, sink: &mut Sink, node: usize, source: &str,
    ) -> String {
        self.sync_down(sink);
        self.vm.set_current_script(self.s2d.get(&node).map(|&d| d as u32));
        // A script that throws is the page's problem, not the parser's;
        // the rest of the document still parses.
        let _ = self.vm.run_source(source);
        self.vm.set_current_script(None);
        self.sync_up(sink);
        self.vm.take_parser_writes()
    }

    // ---- parse tree -> engine DOM -------------------------------------

    fn sync_down(&mut self, sink: &Sink) {
        let root = sink.nodes[sink.document]
            .children
            .iter()
            .copied()
            .find(|&c| sink.is_html_element(c, "html"));
        let Some(root) = root else { return };
        let mut doc = self.doc.borrow_mut();
        if !self.s2d.contains_key(&root) {
            let idx = doc.new_element("html".to_string(), Vec::new(), None);
            doc.root = idx;
            self.s2d.insert(root, idx);
            self.d2s.insert(idx, root);
        }
        let mut stack = vec![root];
        while let Some(s) = stack.pop() {
            let d = self.s2d[&s];
            sync_attrs_down(&mut doc, sink, s, d);
            let mut kids = Vec::new();
            for i in 0..sink.nodes[s].children.len() {
                let c = sink.nodes[s].children[i];
                let dc = match self.s2d.get(&c) {
                    Some(&x) => x,
                    None => match &sink.nodes[c].data {
                        NodeData::Element { name, .. } => {
                            let x = doc.new_element(
                                name.clone(),
                                Vec::new(),
                                None,
                            );
                            self.s2d.insert(c, x);
                            self.d2s.insert(x, c);
                            sync_attrs_down(&mut doc, sink, c, x);
                            x
                        }
                        NodeData::Text(t) => {
                            let x = doc.new_text(t.clone(), d);
                            doc.nodes[d].children.pop();
                            self.s2d.insert(c, x);
                            self.d2s.insert(x, c);
                            x
                        }
                        // comment, doctype, template contents: the
                        // engine's DOM has no place for them
                        _ => continue,
                    },
                };
                if let NodeData::Text(t) = &sink.nodes[c].data {
                    doc.nodes[dc].text = t.clone();
                }
                doc.nodes[dc].parent = Some(d);
                kids.push(dc);
                if matches!(sink.nodes[c].data, NodeData::Element { .. }) {
                    stack.push(c);
                }
            }
            doc.nodes[d].children = kids;
        }
    }

    // ---- engine DOM -> parse tree -------------------------------------

    fn sync_up(&mut self, sink: &mut Sink) {
        let doc = self.doc.borrow();
        if !self.d2s.contains_key(&doc.root) {
            return;
        }
        let mut stack = vec![doc.root];
        while let Some(d) = stack.pop() {
            let s = self.d2s[&d];
            sync_attrs_up(sink, &doc, s, d);
            // Children the parse tree has that the engine's DOM cannot
            // hold are kept, anchored behind whichever mirrored sibling
            // they followed.
            let mut extras: HashMap<Option<usize>, Vec<usize>> =
                HashMap::new();
            let mut anchor = None;
            let old = sink.nodes[s].children.clone();
            for c in &old {
                if self.s2d.contains_key(c) {
                    anchor = Some(*c);
                } else {
                    extras.entry(anchor).or_default().push(*c);
                }
            }
            let mut kids = extras.remove(&None).unwrap_or_default();
            for i in 0..doc.nodes[d].children.len() {
                let dc = doc.nodes[d].children[i];
                let sc = match self.d2s.get(&dc) {
                    Some(&x) => x,
                    None => {
                        let node = &doc.nodes[dc];
                        let data = match &node.tag {
                            Some(tag) => NodeData::Element {
                                ns: Ns::Html,
                                name: tag.clone(),
                                attrs: Vec::new(),
                            },
                            None => NodeData::Text(node.text.clone()),
                        };
                        let x = sink.push(data);
                        self.s2d.insert(x, dc);
                        self.d2s.insert(dc, x);
                        x
                    }
                };
                kids.push(sc);
                kids.extend(extras.remove(&Some(sc)).unwrap_or_default());
                if doc.nodes[dc].is_element() {
                    stack.push(dc);
                }
            }
            for c in &old {
                if !kids.contains(c) {
                    sink.detach(*c);
                }
            }
            for &c in &kids {
                sink.nodes[c].parent = Some(s);
            }
            sink.nodes[s].children = kids;
        }
    }
}

/// The engine spells a namespaced attribute `xlink:href`; the parse tree
/// keeps the prefix and local name apart.
fn flat(name: &AttrName) -> String {
    name.serialized().replace(' ', ":")
}

fn unflat(name: &str) -> AttrName {
    match name.split_once(':') {
        Some((p, l)) if !p.is_empty() && !l.is_empty() => AttrName {
            prefix: Some(p.to_string()),
            local: l.to_string(),
        },
        _ => AttrName::local(name),
    }
}

fn sync_attrs_down(
    doc: &mut dom::Document, sink: &Sink, s: usize, d: usize,
) {
    let want: Vec<(String, String)> = sink
        .attrs(s)
        .iter()
        .map(|(k, v)| (flat(k), v.clone()))
        .collect();
    // set_attr/remove_attr rather than a wholesale replace, so the
    // document's id index stays true.
    let stale: Vec<String> = doc.nodes[d]
        .attrs
        .iter()
        .filter(|(k, _)| !want.iter().any(|(k2, _)| k2 == k))
        .map(|(k, _)| k.clone())
        .collect();
    for k in stale {
        doc.remove_attr(d, &k);
    }
    for (k, v) in want {
        if doc.nodes[d].attr(&k) != Some(v.as_str()) {
            doc.set_attr(d, &k, &v);
        }
    }
}

fn sync_attrs_up(sink: &mut Sink, doc: &dom::Document, s: usize, d: usize) {
    match &mut sink.nodes[s].data {
        NodeData::Element { attrs, .. } => {
            *attrs = doc.nodes[d]
                .attrs
                .iter()
                .map(|(k, v)| (unflat(k), v.clone()))
                .collect();
        }
        NodeData::Text(t) => {
            t.clone_from(&doc.nodes[d].text);
        }
        _ => {}
    }
}
