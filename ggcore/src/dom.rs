//! Flat-arena DOM: nodes live in one Vec, linked by indices.

use std::collections::HashMap;

pub struct Node {
    pub parent: Option<usize>,
    /// None => text node
    pub tag: Option<String>,
    pub text: String,
    pub attrs: Vec<(String, String)>,
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
        self.tag.is_some()
    }
}

pub struct Document {
    pub nodes: Vec<Node>,
    pub root: usize,
    /// id -> candidate node indices in creation order, so
    /// getElementById is O(1) instead of an arena scan.
    pub id_map: HashMap<String, Vec<usize>>,
}

impl Document {
    pub fn with_capacity(cap: usize) -> Document {
        Document {
            nodes: Vec::with_capacity(cap),
            root: 0,
            id_map: HashMap::new(),
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
        let classes = attrs
            .iter()
            .find(|(k, _)| k == "class")
            .map(|(_, v)| v.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default();
        let idx = self.nodes.len();
        self.nodes.push(Node {
            parent,
            tag: Some(tag),
            text: String::new(),
            attrs,
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
        }
        if name == "class" {
            node.classes =
                value.split_whitespace().map(str::to_string).collect();
        }
    }

    pub fn remove_attr(&mut self, idx: usize, name: &str) {
        if name == "id" {
            if let Some(old) = self.nodes[idx].attr("id") {
                let old = old.to_string();
                if let Some(v) = self.id_map.get_mut(&old) {
                    v.retain(|&i| i != idx);
                }
            }
        }
        let node = &mut self.nodes[idx];
        node.attrs.retain(|(k, _)| k != name);
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
            if !node.is_element() {
                out.push_str(&node.text);
            }
            for &c in node.children.iter().rev() {
                stack.push(c);
            }
        }
        out
    }

    /// Detach a node from its parent (if any).
    pub fn detach(&mut self, idx: usize) {
        if let Some(p) = self.nodes[idx].parent {
            self.nodes[p].children.retain(|&c| c != idx);
            self.nodes[idx].parent = None;
        }
    }

    /// Deep-copy a node (and subtree) from another document into self.
    pub fn graft(&mut self, other: &Document, src: usize, parent: usize) {
        let node = &other.nodes[src];
        let new_idx = if node.is_element() {
            self.new_element(
                node.tag.clone().unwrap(),
                node.attrs.clone(),
                Some(parent),
            )
        } else {
            self.new_text(node.text.clone(), parent)
        };
        for &c in &node.children {
            self.graft(other, c, new_idx);
        }
    }

    pub fn new_text(&mut self, text: String, parent: usize) -> usize {
        let idx = self.nodes.len();
        self.nodes.push(Node {
            parent: Some(parent),
            tag: None,
            text,
            attrs: Vec::new(),
            children: Vec::new(),
            style: HashMap::new(),
            classes: Vec::new(),
        });
        self.nodes[parent].children.push(idx);
        idx
    }
}
