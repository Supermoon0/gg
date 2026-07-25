//! Engine-neutral DOM query and serialization helpers.

use crate::dom::Document;

pub(crate) fn query(
    doc: &Document,
    selector_text: &str,
    first_only: bool,
) -> Vec<usize> {
    let selectors = crate::css::parse_selector_list(selector_text);
    if selectors.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut stack = vec![doc.root];
    while let Some(idx) = stack.pop() {
        let node = &doc.nodes[idx];
        if node.is_element()
            && selectors.iter().any(|s| s.matches(doc, idx))
        {
            out.push(idx);
            if first_only {
                return out;
            }
        }
        for &child in node.children.iter().rev() {
            stack.push(child);
        }
    }
    out
}

/// `query`, but restricted to the descendants of `root`.
///
/// Selectors still match against the whole document (a descendant
/// combinator may reach above `root`, exactly as in a browser); only
/// the *results* are limited to the subtree.
pub(crate) fn query_within(
    doc: &Document,
    root: usize,
    selector_text: &str,
    first_only: bool,
) -> Vec<usize> {
    if root == doc.root {
        return query(doc, selector_text, first_only);
    }
    let mut out = Vec::new();
    for idx in query(doc, selector_text, false) {
        let mut cur = doc.nodes[idx].parent;
        while let Some(p) = cur {
            if p == root {
                out.push(idx);
                if first_only {
                    return out;
                }
                break;
            }
            cur = doc.nodes[p].parent;
        }
    }
    out
}

pub(crate) fn find_tag(doc: &Document, tag: &str) -> Option<usize> {
    doc.nodes
        .iter()
        .position(|node| node.tag.as_deref() == Some(tag))
}

pub(crate) fn serialize_children(doc: &Document, idx: usize) -> String {
    let mut out = String::new();
    for &child in &doc.nodes[idx].children {
        serialize(doc, child, &mut out);
    }
    out
}

fn serialize(doc: &Document, idx: usize, out: &mut String) {
    let node = &doc.nodes[idx];
    match &node.tag {
        None => out.push_str(
            &node
                .text
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;"),
        ),
        Some(tag) => {
            out.push('<');
            out.push_str(tag);
            for (name, value) in &node.attrs {
                out.push(' ');
                out.push_str(name);
                out.push_str("=\"");
                out.push_str(&value.replace('"', "&quot;"));
                out.push('"');
            }
            out.push('>');
            for &child in &node.children {
                serialize(doc, child, out);
            }
            out.push_str("</");
            out.push_str(tag);
            out.push('>');
        }
    }
}
