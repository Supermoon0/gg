//! PyO3 bindings: the Python engine calls into this native core for
//! HTML parsing, CSS parsing, and style computation.

use std::cell::RefCell;
use std::rc::Rc;

use pyo3::prelude::*;
use pyo3::types::PyBytes;

mod css;
mod dom;
mod fonts;
mod html;
mod js;
mod jsvm;
mod raster;
mod style;
mod svg;
mod window;

/// (kind, x1, y1, x2, y2, rgb, aux, font_id, text)
/// kind: 0=rect 1=text(aux=size) 2=line(aux=thickness) 3=oval 4=image
type Cmd = (u8, f64, f64, f64, f64, (u8, u8, u8), f64, u32, String);

#[pyclass]
struct TextEngine {
    store: fonts::FontStore,
    images: std::collections::HashMap<u32, (u32, u32, Vec<u8>)>,
    next_image: u32,
}

#[pymethods]
impl TextEngine {
    #[new]
    fn new() -> PyResult<Self> {
        fonts::FontStore::new()
            .map(|store| TextEngine {
                store,
                images: std::collections::HashMap::new(),
                next_image: 1,
            })
            .map_err(pyo3::exceptions::PyRuntimeError::new_err)
    }

    /// Decode an image; returns (image_id, width, height).
    fn load_image(&mut self, data: &[u8]) -> PyResult<(u32, u32, u32)> {
        let img = image::load_from_memory(data).map_err(|e| {
            pyo3::exceptions::PyValueError::new_err(format!(
                "image decode failed: {e}"
            ))
        })?;
        let rgba = img.to_rgba8();
        let (w, h) = (rgba.width(), rgba.height());
        let id = self.next_image;
        self.next_image += 1;
        self.images.insert(id, (w, h, rgba.into_raw()));
        Ok((id, w, h))
    }

    fn clear_images(&mut self) {
        self.images.clear();
    }

    /// Rasterize inline-SVG path data into the image store.
    /// paths: [(d_attribute, (r, g, b)), ...] painted in order.
    /// Returns (image_id, width, height) like load_image.
    fn load_svg(
        &mut self,
        view_box: (f64, f64, f64, f64),
        out_w: u32,
        out_h: u32,
        paths: Vec<(String, (u8, u8, u8))>,
    ) -> (u32, u32, u32) {
        // icons only: keep the supersampled fill grid small
        let w = out_w.clamp(1, 512);
        let h = out_h.clamp(1, 512);
        let rgba = svg::rasterize(view_box, w as usize, h as usize, &paths);
        let id = self.next_image;
        self.next_image += 1;
        self.images.insert(id, (w, h, rgba));
        (id, w, h)
    }

    fn font_id(&mut self, family: &str, bold: bool, italic: bool) -> u32 {
        self.store.variant_id(family, bold, italic)
    }

    /// (ascent, descent, linespace) at a pixel size.
    fn metrics(&mut self, font_id: u32, size: f32) -> (f32, f32, f32) {
        self.store.metrics(font_id, size)
    }

    fn measure(&mut self, font_id: u32, size: f32, text: &str) -> f32 {
        self.store.measure(font_id, size, text)
    }

    /// Rasterize a display list; returns a binary PPM (P6) image.
    fn render<'py>(
        &mut self,
        py: Python<'py>,
        width: u32,
        height: u32,
        bg: (u8, u8, u8),
        cmds: Vec<Cmd>,
    ) -> Bound<'py, PyBytes> {
        let r = self.rasterize(width, height, bg, &cmds);
        PyBytes::new(py, &r.to_ppm())
    }

    /// Rasterize a display list; returns raw RGB bytes (no header).
    fn render_raw<'py>(
        &mut self,
        py: Python<'py>,
        width: u32,
        height: u32,
        bg: (u8, u8, u8),
        cmds: Vec<Cmd>,
    ) -> Bound<'py, PyBytes> {
        let r = self.rasterize(width, height, bg, &cmds);
        PyBytes::new(py, &r.buf)
    }
}

impl TextEngine {
    fn rasterize(
        &mut self,
        width: u32,
        height: u32,
        bg: (u8, u8, u8),
        cmds: &[Cmd],
    ) -> raster::Raster {
        let mut r = raster::Raster::new(
            width.max(1) as usize,
            height.max(1) as usize,
            bg,
        );
        let mut clip_stack: Vec<(i32, i32, i32, i32)> = Vec::new();
        for (kind, x1, y1, x2, y2, color, aux, font, text) in cmds {
            match kind {
                // rect; aux is the border-radius (0 = square)
                0 => {
                    if *aux > 0.5 {
                        r.fill_round_rect(*x1, *y1, *x2, *y2, *color, *aux);
                    } else {
                        r.fill_rect(*x1, *y1, *x2, *y2, *color);
                    }
                }
                1 => r.draw_text(
                    &mut self.store, *x1, *y1, *font, *aux as f32, *color,
                    text,
                ),
                2 => r.draw_line(*x1, *y1, *x2, *y2, *color, *aux),
                3 => r.fill_oval(*x1, *y1, *x2, *y2, *color),
                // kind 4: image — x2/y2 are target w/h, font is image id
                4 => {
                    if let Some((sw, sh, rgba)) = self.images.get(font) {
                        r.draw_image(*sw, *sh, rgba, *x1, *y1, *x2, *y2);
                    }
                }
                // kind 5: background layer — x2/y2 are the box w/h,
                // font is the image id, text carries
                // "off_x off_y tile_w tile_h rep_x rep_y"
                5 => {
                    if let Some((sw, sh, rgba)) = self.images.get(font) {
                        let p: Vec<f64> = text
                            .split_whitespace()
                            .filter_map(|t| t.parse().ok())
                            .collect();
                        if p.len() == 6 {
                            r.draw_image_tiled(
                                *sw, *sh, rgba, *x1, *y1, *x2, *y2,
                                p[0], p[1], p[2], p[3],
                                p[4] != 0.0, p[5] != 0.0,
                            );
                        }
                    }
                }
                // kind 6: push clip (x1,y1,x2,y2 = rect); kind 7: pop
                6 => clip_stack.push(r.push_clip(*x1, *y1, *x2, *y2)),
                7 => {
                    if let Some(prev) = clip_stack.pop() {
                        r.set_clip(prev);
                    }
                }
                _ => {}
            }
        }
        r
    }
}

#[pyclass(unsendable)]
struct NativeWindow {
    inner: window::NativeWindowInner,
}

#[pymethods]
impl NativeWindow {
    #[new]
    fn new(width: u32, height: u32, title: String) -> PyResult<Self> {
        window::NativeWindowInner::new(width, height, title)
            .map(|inner| NativeWindow { inner })
            .map_err(pyo3::exceptions::PyRuntimeError::new_err)
    }

    /// Pump the event loop; returns (kind, a, b, text) event tuples.
    fn pump(&mut self, timeout_ms: u64) -> Vec<window::Event> {
        self.inner.pump(timeout_ms)
    }

    /// Blit a raw RGB frame (from render_raw) to the window.
    fn present(&mut self, width: u32, height: u32, rgb: &[u8]) {
        self.inner.app.present(width, height, rgb);
    }

    /// Physical (device px) size; pass this to present().
    fn size(&self) -> (u32, u32) {
        self.inner.app.size
    }

    /// DPI scale factor: device px per logical/CSS px.
    fn scale_factor(&self) -> f64 {
        self.inner.app.scale
    }

    fn set_title(&mut self, title: String) {
        self.inner.set_title(title);
    }

    fn set_cursor_pointer(&mut self, pointer: bool) {
        self.inner.set_cursor_pointer(pointer);
    }
}

type ExportedNode = (
    i64,                        // parent index in exported order (-1 = root)
    u64,                        // node index in the Rust arena (for events)
    Option<String>,             // tag (None => text node)
    Option<String>,             // text (None => element)
    Vec<(String, String)>,      // attributes
    Vec<(String, String)>,      // computed style
);

/// One semantic node for an AI agent — the compact projection that
/// replaces a full-DOM marshal (the AI-native primitive). Computed
/// entirely in Rust so the agent never round-trips the whole tree.
type SnapNode = (
    u64,             // ridx (stable arena index; use with dispatch_click)
    String,          // ARIA/implicit role
    String,          // tag name
    String,          // accessible name (visible text; script/style/hidden skipped)
    Option<String>,  // href
    Option<String>,  // input type
    Option<String>,  // id
    bool,            // interactive
);

/// A selector match: enough to build a driver Element handle without a
/// whole-tree export (ridx, tag, attrs, visible text).
type QueryNode = (u64, String, Vec<(String, String)>, String);

const RAW_TEXT_TAGS: [&str; 4] = ["script", "style", "template", "noscript"];

fn is_interactive(tag: &str) -> bool {
    matches!(tag, "a" | "button" | "input" | "select" | "textarea" | "option")
}

/// Implicit ARIA role (explicit role attr wins). None => not surfaced
/// in the snapshot. Mirrors browser/driver.py::_implicit_role.
fn implicit_role(node: &dom::Node, tag: &str) -> Option<String> {
    if let Some(r) = node.attr("role") {
        return Some(r.to_string());
    }
    let role = match tag {
        "a" if node.attr("href").is_some() => "link",
        "button" => "button",
        "input" => match node.attr("type").unwrap_or("text") {
            "checkbox" => "checkbox",
            "radio" => "radio",
            "submit" | "button" | "reset" => "button",
            "hidden" => return None,
            _ => "textbox",
        },
        "select" => "combobox",
        "textarea" => "textbox",
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => "heading",
        "nav" => "navigation",
        _ => return None,
    };
    Some(role.to_string())
}

/// Visible descendant text: like textContent but skips script/style/
/// template/noscript subtrees and display:none nodes. Iterative so a
/// pathologically deep tree cannot overflow the stack.
fn visible_text(doc: &dom::Document, root: usize) -> String {
    let mut out = String::new();
    let mut stack = vec![root];
    while let Some(idx) = stack.pop() {
        let node = &doc.nodes[idx];
        match node.tag.as_deref() {
            Some(tag) => {
                if RAW_TEXT_TAGS.contains(&tag) {
                    continue;
                }
                if node.style.get("display").map(String::as_str) == Some("none")
                {
                    continue;
                }
            }
            None => out.push_str(&node.text),
        }
        for &c in node.children.iter().rev() {
            stack.push(c);
        }
    }
    out
}

fn truncate_chars(s: &str, max: usize) -> String {
    let t = s.trim();
    match t.char_indices().nth(max) {
        Some((byte_idx, _)) => t[..byte_idx].to_string(),
        None => t.to_string(),
    }
}

#[pyclass(unsendable)]
struct Doc {
    doc: Rc<RefCell<dom::Document>>,
    js: Option<boa_engine::Context>,
    ggjs: Option<jsvm::page::PageVm>,
    /// GGJS=1 routes scripts/events to the hand-written gg-js engine
    use_ggjs: bool,
}

#[pyfunction]
fn parse_html(html: &str) -> Doc {
    Doc {
        doc: Rc::new(RefCell::new(html::parse(html))),
        js: None,
        ggjs: None,
        use_ggjs: std::env::var("GGJS").ok().as_deref() == Some("1"),
    }
}

impl Doc {
    fn ggvm(&mut self) -> &mut jsvm::page::PageVm {
        if self.ggjs.is_none() {
            self.ggjs =
                Some(jsvm::page::PageVm::new(Some(self.doc.clone())));
        }
        self.ggjs.as_mut().unwrap()
    }
}

#[pymethods]
impl Doc {
    fn node_count(&self) -> usize {
        self.doc.borrow().nodes.len()
    }

    /// ("inline", css_text) and ("link", href) entries in document order.
    fn stylesheet_entries(&self) -> Vec<(String, String)> {
        let doc = self.doc.borrow();
        let mut out = Vec::new();
        let mut stack = vec![doc.root];
        while let Some(idx) = stack.pop() {
            let node = &doc.nodes[idx];
            match node.tag.as_deref() {
                Some("style") => {
                    let css: String = node
                        .children
                        .iter()
                        .map(|&c| doc.nodes[c].text.as_str())
                        .collect();
                    out.push(("inline".to_string(), css));
                }
                Some("link") => {
                    let rel = node.attr("rel").unwrap_or("");
                    if rel.eq_ignore_ascii_case("stylesheet") {
                        if let Some(href) = node.attr("href") {
                            out.push(("link".to_string(), href.to_string()));
                        }
                    }
                }
                _ => {}
            }
            for &c in node.children.iter().rev() {
                stack.push(c);
            }
        }
        out
    }

    /// ("inline", code) and ("src", url) entries in document order.
    fn script_entries(&self) -> Vec<(String, String)> {
        let doc = self.doc.borrow();
        let mut out = Vec::new();
        let mut stack = vec![doc.root];
        while let Some(idx) = stack.pop() {
            let node = &doc.nodes[idx];
            if node.tag.as_deref() == Some("script") {
                let stype = node.attr("type").unwrap_or("").to_lowercase();
                if stype.is_empty()
                    || stype.contains("javascript")
                    || stype == "module"
                {
                    if let Some(src) = node.attr("src") {
                        out.push(("src".to_string(), src.to_string()));
                    } else {
                        let code: String = node
                            .children
                            .iter()
                            .map(|&c| doc.nodes[c].text.as_str())
                            .collect();
                        if !code.trim().is_empty() {
                            out.push(("inline".to_string(), code));
                        }
                    }
                }
            }
            for &c in node.children.iter().rev() {
                stack.push(c);
            }
        }
        out
    }

    /// Tell the JS engine the page URL so `location.*` is real.
    /// Call before run_scripts. No-op on the Boa path.
    fn set_page_url(&mut self, url: String) {
        if self.use_ggjs {
            self.ggvm().set_page_url(&url);
        }
    }

    /// One real-time slice of the live event loop: fires timers/rAF
    /// due within the next dt_ms of virtual time. Returns (console
    /// output, fetches to service). The render loop calls this
    /// periodically after load.
    fn tick(&mut self, dt_ms: f64) -> (Vec<String>, Vec<(u32, String)>) {
        if self.use_ggjs {
            self.ggvm().tick(dt_ms)
        } else {
            (Vec::new(), Vec::new())
        }
    }

    /// DOM mutation counter — re-style/re-layout only when it changes.
    fn dom_version(&self) -> u64 {
        self.doc.borrow().version
    }

    /// Fire DOMContentLoaded / load once all scripts have run — app
    /// bundles bootstrap from these. Returns console output.
    fn fire_lifecycle(&mut self) -> Vec<String> {
        if self.use_ggjs {
            self.ggvm().fire_lifecycle()
        } else {
            Vec::new()
        }
    }

    /// (listeners, timers, microtasks) for boot diagnosis.
    fn pending_counts(&mut self) -> (usize, usize, usize) {
        if self.use_ggjs {
            self.ggvm().pending_counts()
        } else {
            (0, 0, 0)
        }
    }

    /// Run scripts (in order) against the DOM. Returns console output.
    /// The JS context persists, so later events see earlier definitions.
    fn run_scripts(&mut self, sources: Vec<String>) -> Vec<String> {
        if self.use_ggjs {
            return self.ggvm().run_scripts(&sources);
        }
        if self.js.is_none() {
            self.js = Some(js::new_context());
        }
        js::run(self.js.as_mut().unwrap(), self.doc.clone(), &sources)
    }

    /// Bubble a click through onclick attributes + addEventListener
    /// handlers. Returns (console output, whether any handler ran,
    /// whether the default action was prevented).
    fn dispatch_click(
        &mut self,
        node_idx: usize,
    ) -> (Vec<String>, bool, bool) {
        if self.use_ggjs {
            return self.ggvm().dispatch_click(node_idx);
        }
        if self.js.is_none() {
            self.js = Some(js::new_context());
        }
        js::dispatch_click(
            self.js.as_mut().unwrap(),
            self.doc.clone(),
            node_idx,
        )
    }

    /// Async runtime (P3, gg-js only): run the event loop to a fixed
    /// point. Returns (console output, [(fetch_id, url)] to service).
    /// No-op on the Boa path (Boa has its own loop).
    fn pump(&mut self) -> (Vec<String>, Vec<(u32, String)>) {
        if !self.use_ggjs {
            return (Vec::new(), Vec::new());
        }
        self.ggvm().pump()
    }

    /// Host settles a fetch the driver performed (gg-js only).
    fn resolve_fetch(&mut self, fetch_id: u32, status: u16, body: String) {
        if self.use_ggjs {
            self.ggvm().resolve_fetch(fetch_id, status, body);
        }
    }

    fn reject_fetch(&mut self, fetch_id: u32, message: String) {
        if self.use_ggjs {
            self.ggvm().reject_fetch(fetch_id, message);
        }
    }

    /// Whether the event loop still has queued microtasks/timers/fetches.
    fn has_pending_work(&self) -> bool {
        self.ggjs.as_ref().is_some_and(|vm| vm.has_pending_work())
    }

    /// Run an event-handler attribute (e.g. "onclick") of a node.
    fn run_event(&mut self, node_idx: usize, attr: String) -> Vec<String> {
        let code = self
            .doc
            .borrow()
            .nodes
            .get(node_idx)
            .and_then(|n| n.attr(&attr))
            .map(str::to_string);
        match code {
            Some(code) => self.run_scripts(vec![code]),
            None => Vec::new(),
        }
    }

    /// css_sources are parsed in order (UA sheet first, then page css).
    /// viewport_width drives @media (min/max-width) evaluation.
    #[pyo3(signature = (css_sources, viewport_width=1280.0))]
    fn compute_styles(&mut self, css_sources: Vec<String>,
                      viewport_width: f64) {
        style::compute_styles_vw(
            &mut self.doc.borrow_mut(), &css_sources, viewport_width);
    }

    /// Flat pre-order dump; Python rebuilds its Element/Text tree from it.
    fn export(&self) -> Vec<ExportedNode> {
        let doc = self.doc.borrow();
        let n = doc.nodes.len();
        let mut out: Vec<ExportedNode> = Vec::with_capacity(n);
        let mut map = vec![usize::MAX; n];
        let mut stack = vec![doc.root];
        while let Some(idx) = stack.pop() {
            let node = &doc.nodes[idx];
            map[idx] = out.len();
            let parent = match node.parent {
                Some(p) if map[p] != usize::MAX => map[p] as i64,
                _ => -1,
            };
            let (tag, text) = match &node.tag {
                Some(t) => (Some(t.clone()), None),
                None => (None, Some(node.text.clone())),
            };
            let style_pairs = node
                .style
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            out.push((
                parent,
                idx as u64,
                tag,
                text,
                node.attrs.clone(),
                style_pairs,
            ));
            for &c in node.children.iter().rev() {
                stack.push(c);
            }
        }
        out
    }

    /// Compact semantic snapshot for an AI agent: one entry per node
    /// that has a role, with a pre-computed accessible name, in document
    /// order. This is the AI-native primitive — it lets the agent read
    /// the page's actionable structure without marshaling the whole DOM
    /// into Python (which export() does).
    fn snapshot(&self) -> Vec<SnapNode> {
        let doc = self.doc.borrow();
        let mut out = Vec::new();
        let mut stack = vec![doc.root];
        while let Some(idx) = stack.pop() {
            let node = &doc.nodes[idx];
            if let Some(tag) = node.tag.as_deref() {
                if let Some(role) = implicit_role(node, tag) {
                    out.push((
                        idx as u64,
                        role,
                        tag.to_string(),
                        truncate_chars(&visible_text(&doc, idx), 120),
                        node.attr("href").map(str::to_string),
                        node.attr("type").map(str::to_string),
                        node.attr("id").map(str::to_string),
                        is_interactive(tag),
                    ));
                }
            }
            for &c in node.children.iter().rev() {
                stack.push(c);
            }
        }
        out
    }

    /// Resolve a CSS selector to matching nodes using the engine's own
    /// matcher, returning (ridx, tag, attrs, visible-text) per match in
    /// document order — enough to build a driver handle with no export.
    /// `first` stops at the first match.
    fn query(&self, selector: &str, first: bool) -> Vec<QueryNode> {
        let doc = self.doc.borrow();
        let selectors = css::parse_selector_list(selector);
        if selectors.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut stack = vec![doc.root];
        while let Some(idx) = stack.pop() {
            let node = &doc.nodes[idx];
            if node.is_element()
                && selectors.iter().any(|s| s.matches(&doc, idx))
            {
                out.push((
                    idx as u64,
                    node.tag.clone().unwrap(),
                    node.attrs.clone(),
                    truncate_chars(&visible_text(&doc, idx), 200),
                ));
                if first {
                    return out;
                }
            }
            for &c in node.children.iter().rev() {
                stack.push(c);
            }
        }
        out
    }
}

/// Run a script on the experimental gg-js VM and return the value of
/// its last expression statement (numbers only for now).
#[pyfunction]
fn jsvm_eval(src: &str) -> PyResult<f64> {
    match jsvm::eval(src) {
        Ok((v, _)) if v.is_number() => Ok(v.to_number_raw()),
        Ok((v, _)) => Err(pyo3::exceptions::PyValueError::new_err(format!(
            "non-number result: {v:?}"
        ))),
        Err(e) => Err(pyo3::exceptions::PyValueError::new_err(e)),
    }
}

/// Run a script on the gg-js VM and return its console.log output —
/// the same contract as Doc.run_scripts, so benches run on either.
#[pyfunction]
fn jsvm_run(src: &str) -> PyResult<Vec<String>> {
    match jsvm::eval(src) {
        Ok((_, logs)) => Ok(logs),
        Err(e) => Err(pyo3::exceptions::PyValueError::new_err(e)),
    }
}

#[pymodule]
fn ggcore(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Doc>()?;
    m.add_class::<TextEngine>()?;
    m.add_class::<NativeWindow>()?;
    m.add_function(wrap_pyfunction!(parse_html, m)?)?;
    m.add_function(wrap_pyfunction!(jsvm_eval, m)?)?;
    m.add_function(wrap_pyfunction!(jsvm_run, m)?)?;
    Ok(())
}
