//! PyO3 bindings: the Python engine calls into this native core for
//! HTML parsing, CSS parsing, and style computation.

use std::cell::RefCell;
use std::rc::Rc;

use pyo3::prelude::*;
use pyo3::types::PyBytes;

mod css;
mod dom;
mod dom_api;
mod fonts;
mod html;
mod jsvm;
mod raster;
mod style;
mod svg;
mod window;

/// (kind, x1, y1, x2, y2, rgb, aux, font_id, text)
/// kind: 0=rect 1=text(aux=size) 2=line(aux=thickness) 3=oval 4=image,
/// 6/7=clip push/pop, 8/9=vertical sticky push/pop
type Cmd = (u8, f64, f64, f64, f64, (u8, u8, u8), f64, u32, String);

#[pyclass]
struct TextEngine {
    store: fonts::FontStore,
    images: std::collections::HashMap<u32, (u32, u32, Vec<u8>)>,
    next_image: u32,
    /// The page display list in document coordinates, stored once per
    /// layout/paint change so scroll frames pass only offsets instead
    /// of re-serializing and re-transferring every command (M6).
    display_list: Vec<Cmd>,
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
                display_list: Vec::new(),
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

    /// Register a web font (@font-face) under a family name.
    /// TTF/OTF only (fontdue); returns false on unparsable data.
    fn load_font(
        &mut self,
        family: &str,
        bold: bool,
        italic: bool,
        data: &[u8],
    ) -> bool {
        self.store.add_font(family, bold, italic, data.to_vec())
    }

    fn has_family(&self, family: &str) -> bool {
        self.store.has_family(family)
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

    /// Store the page display list (document coordinates; device px
    /// if the shell pre-scales). Call on layout/paint changes only.
    fn set_display_list(&mut self, cmds: Vec<Cmd>) {
        self.display_list = cmds;
    }

    /// Rasterize the stored list at a scroll offset, then draw the
    /// overlay (chrome — already in viewport coordinates) on top.
    /// Off-viewport commands are culled here; clip brackets always
    /// survive so push/pop stay balanced. Returns a binary PPM.
    #[allow(clippy::too_many_arguments)]
    fn render_frame<'py>(
        &mut self,
        py: Python<'py>,
        width: u32,
        height: u32,
        bg: (u8, u8, u8),
        dx: f64,
        dy: f64,
        overlay: Vec<Cmd>,
    ) -> Bound<'py, PyBytes> {
        let r = self.rasterize_at(width, height, bg, dx, dy, &overlay);
        PyBytes::new(py, &r.to_ppm())
    }

    /// As render_frame, returning raw RGB bytes (native shell).
    #[allow(clippy::too_many_arguments)]
    fn render_frame_raw<'py>(
        &mut self,
        py: Python<'py>,
        width: u32,
        height: u32,
        bg: (u8, u8, u8),
        dx: f64,
        dy: f64,
        overlay: Vec<Cmd>,
    ) -> Bound<'py, PyBytes> {
        let r = self.rasterize_at(width, height, bg, dx, dy, &overlay);
        PyBytes::new(py, &r.buf)
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
    /// Shift a command into viewport space and report whether any of
    /// it can be visible. x2/y2 shift only when they are coordinates
    /// (rect/line/oval/clip) — for images and background layers they
    /// are width/height. Clip brackets are never culled.
    fn shift_cull(
        cmd: &Cmd,
        dx: f64,
        dy: f64,
        width: f64,
        height: f64,
    ) -> Option<Cmd> {
        let (kind, x1, y1, x2, y2, color, aux, font, text) = cmd;
        let (kind, x1, y1, x2, y2) = (*kind, *x1, *y1, *x2, *y2);
        match kind {
            6 | 7 => Some((
                kind,
                x1 - dx,
                y1 - dy,
                x2 - dx,
                y2 - dy,
                *color,
                *aux,
                *font,
                text.clone(),
            )),
            _ => {
                let (top, bottom, left, right) = match kind {
                    // text: y extent from the font size (linespace is
                    // ~1.3x; 2x keeps the cull conservative)
                    1 => (y1, y1 + aux * 2.0, x1, f64::INFINITY),
                    // image / background layer: x2/y2 are w/h
                    4 | 5 => (y1, y1 + y2, x1, x1 + x2),
                    _ => (
                        y1.min(y2),
                        y1.max(y2),
                        x1.min(x2),
                        x1.max(x2),
                    ),
                };
                if bottom < dy
                    || top > dy + height
                    || right < dx
                    || left > dx + width
                {
                    return None;
                }
                let shifts_wh = !matches!(kind, 4 | 5);
                Some((
                    kind,
                    x1 - dx,
                    y1 - dy,
                    if shifts_wh { x2 - dx } else { x2 },
                    if shifts_wh { y2 - dy } else { y2 },
                    *color,
                    *aux,
                    *font,
                    text.clone(),
                ))
            }
        }
    }

    /// Translate a display command in document space. Image/background
    /// x2/y2 fields are dimensions; every other drawable uses endpoints.
    fn translate_cmd(cmd: &Cmd, dx: f64, dy: f64) -> Cmd {
        let (kind, x1, y1, x2, y2, color, aux, font, text) = cmd;
        let shifts_wh = !matches!(*kind, 4 | 5);
        (
            *kind,
            *x1 + dx,
            *y1 + dy,
            if shifts_wh { *x2 + dx } else { *x2 },
            if shifts_wh { *y2 + dy } else { *y2 },
            *color,
            *aux,
            *font,
            text.clone(),
        )
    }

    /// Resolve sticky groups and cull the stored document-space list into
    /// viewport-space commands. Kind 8 stores normal_top in y1, max_top in
    /// y2, and the top inset in aux; kind 9 closes the group.
    fn viewport_cmds(
        list: &[Cmd],
        dx: f64,
        dy: f64,
        width: f64,
        height: f64,
    ) -> Vec<Cmd> {
        let mut out = Vec::with_capacity(list.len() / 4);
        let mut sticky_stack: Vec<f64> = Vec::new();
        let mut sticky_dy = 0.0;
        for cmd in list {
            match cmd.0 {
                8 => {
                    let normal = cmd.2 + sticky_dy;
                    let maximum = cmd.4 + sticky_dy;
                    let stuck = (dy + cmd.6).max(normal).min(maximum);
                    let delta = stuck - normal;
                    sticky_stack.push(delta);
                    sticky_dy += delta;
                }
                9 => {
                    if let Some(delta) = sticky_stack.pop() {
                        sticky_dy -= delta;
                    }
                }
                _ => {
                    let shifted;
                    let candidate = if sticky_dy != 0.0 {
                        shifted = Self::translate_cmd(cmd, 0.0, sticky_dy);
                        &shifted
                    } else {
                        cmd
                    };
                    if let Some(c) =
                        Self::shift_cull(candidate, dx, dy, width, height)
                    {
                        out.push(c);
                    }
                }
            }
        }
        out
    }

    fn rasterize_at(
        &mut self,
        width: u32,
        height: u32,
        bg: (u8, u8, u8),
        dx: f64,
        dy: f64,
        overlay: &[Cmd],
    ) -> raster::Raster {
        let list = std::mem::take(&mut self.display_list);
        let mut cmds = Self::viewport_cmds(
            &list, dx, dy, width as f64, height as f64,
        );
        cmds.reserve(overlay.len());
        cmds.extend_from_slice(overlay);
        let r = self.rasterize(width, height, bg, &cmds);
        self.display_list = list;
        r
    }

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

/// Properties that can never move a box: a restyle touching only
/// these skips relayout (conservative — unknown props count as
/// geometry).
fn is_paint_only_prop(k: &str) -> bool {
    matches!(
        k,
        "color"
            | "background"
            | "background-color"
            | "background-image"
            | "background-position"
            | "background-size"
            | "background-repeat"
            | "text-decoration"
            | "border-color"
            | "border-top-color"
            | "border-right-color"
            | "border-bottom-color"
            | "border-left-color"
            | "outline"
            | "outline-color"
            | "box-shadow"
            | "opacity"
            | "visibility"
            | "cursor"
            | "z-index"
            | "border-radius"
            | "transform"
            | "transition"
            | "animation"
            | "text-overflow"
            | "caret-color"
            | "fill"
            | "stroke"
    )
}

fn is_interactive(tag: &str) -> bool {
    matches!(tag, "a" | "button" | "input" | "select" | "textarea" | "option")
}

/// Accessibility-hidden: this node (not its ancestors) is removed from
/// the accessibility projection, subtree included. Mirrors
/// browser/accessibility.py::is_hidden.
fn ax_hidden(node: &dom::Node) -> bool {
    if node.style.get("display").map(String::as_str) == Some("none") {
        return true;
    }
    if matches!(
        node.style.get("visibility").map(|v| v.trim()),
        Some("hidden") | Some("collapse")
    ) {
        return true;
    }
    if node.attr("hidden").is_some() {
        return true;
    }
    if node
        .attr("aria-hidden")
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
    {
        return true;
    }
    node.tag.as_deref() == Some("input")
        && node.attr("type").is_some_and(|t| t.eq_ignore_ascii_case("hidden"))
}

fn in_sectioning_content(doc: &dom::Document, idx: usize) -> bool {
    let mut cur = doc.nodes[idx].parent;
    while let Some(p) = cur {
        if matches!(
            doc.nodes[p].tag.as_deref(),
            Some("article" | "aside" | "main" | "nav" | "section")
        ) {
            return true;
        }
        cur = doc.nodes[p].parent;
    }
    false
}

fn has_accessible_label(node: &dom::Node) -> bool {
    ["aria-label", "aria-labelledby", "title"]
        .iter()
        .any(|a| node.attr(a).is_some_and(|v| !v.trim().is_empty()))
}

/// Implicit ARIA role (explicit role attr wins; presentation/none drop
/// the node). None => not surfaced in the snapshot. Mirrors
/// browser/accessibility.py::role_of.
fn implicit_role(doc: &dom::Document, idx: usize) -> Option<String> {
    let node = &doc.nodes[idx];
    let tag = node.tag.as_deref()?;
    if let Some(r) = node.attr("role") {
        let first = r.split_whitespace().next().unwrap_or("");
        if first.eq_ignore_ascii_case("presentation")
            || first.eq_ignore_ascii_case("none")
        {
            return None;
        }
        if !first.is_empty() {
            return Some(first.to_ascii_lowercase());
        }
    }
    let role = match tag {
        "a" if node.attr("href").is_some() => "link",
        "button" | "summary" => "button",
        "input" => match node.attr("type").unwrap_or("text") {
            "checkbox" => "checkbox",
            "radio" => "radio",
            "submit" | "button" | "reset" | "image" => "button",
            "range" => "slider",
            "number" => "spinbutton",
            "search" => "searchbox",
            "hidden" => return None,
            _ => "textbox",
        },
        "select" if node.attr("multiple").is_some() => "listbox",
        "select" => "combobox",
        "option" => "option",
        "textarea" => "textbox",
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => "heading",
        "nav" => "navigation",
        "main" => "main",
        "aside" => "complementary",
        "header" if !in_sectioning_content(doc, idx) => "banner",
        "footer" if !in_sectioning_content(doc, idx) => "contentinfo",
        "form" if has_accessible_label(node) => "form",
        "section" if has_accessible_label(node) => "region",
        "ul" | "ol" => "list",
        "li" => "listitem",
        "table" => "table",
        "tr" => "row",
        "td" => "cell",
        "th" if node.attr("scope").is_some_and(|s| s.eq_ignore_ascii_case("row")) => {
            "rowheader"
        }
        "th" => "columnheader",
        "caption" => "caption",
        "img" => match node.attr("alt") {
            Some(alt) if alt.trim().is_empty() => return None,
            _ => "img",
        },
        "article" => "article",
        "dialog" => "dialog",
        "fieldset" => "group",
        "progress" => "progressbar",
        _ => return None,
    };
    Some(role.to_string())
}

/// Visible descendant text: like textContent but skips script/style/
/// template/noscript subtrees and accessibility-hidden nodes, and
/// substitutes alt text for images. Iterative so a pathologically deep
/// tree cannot overflow the stack.
fn visible_text(doc: &dom::Document, root: usize) -> String {
    let mut out = String::new();
    let mut stack = vec![root];
    while let Some(idx) = stack.pop() {
        let node = &doc.nodes[idx];
        match node.tag.as_deref() {
            Some(tag) => {
                if RAW_TEXT_TAGS.contains(&tag) || ax_hidden(node) {
                    continue;
                }
                if tag == "img" {
                    if let Some(alt) = node.attr("alt") {
                        if !alt.trim().is_empty() {
                            out.push(' ');
                            out.push_str(alt);
                            out.push(' ');
                        }
                    }
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

/// textContent of a subtree with no visibility filtering — used for
/// aria-labelledby targets, which may legitimately be hidden.
fn raw_text(doc: &dom::Document, root: usize) -> String {
    let mut out = String::new();
    let mut stack = vec![root];
    while let Some(idx) = stack.pop() {
        let node = &doc.nodes[idx];
        if node.tag.is_none() {
            out.push_str(&node.text);
        }
        for &c in node.children.iter().rev() {
            stack.push(c);
        }
    }
    out
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Roles whose accessible name may come from their contents.
fn name_from_content(role: &str) -> bool {
    matches!(
        role,
        "button" | "link" | "heading" | "cell" | "columnheader"
            | "rowheader" | "option" | "listitem" | "checkbox" | "radio"
            | "menuitem" | "tab" | "caption" | "switch"
    )
}

/// WAI-ARIA accessible-name subset: aria-labelledby -> aria-label ->
/// native markup (alt, label[for]/ancestor label, button value,
/// caption/legend, placeholder) -> content text (for name-from-content
/// roles) -> title. Mirrors browser/accessibility.py::accessible_name.
fn accessible_name(
    doc: &dom::Document,
    idx: usize,
    role: &str,
    ids: &std::collections::HashMap<String, usize>,
    labels: &std::collections::HashMap<String, usize>,
) -> String {
    let node = &doc.nodes[idx];
    if let Some(refs) = node.attr("aria-labelledby") {
        let mut parts: Vec<String> = Vec::new();
        for r in refs.split_whitespace() {
            if let Some(&target) = ids.get(r) {
                let t = &doc.nodes[target];
                let label = t.attr("aria-label").unwrap_or("").trim();
                let text = if !label.is_empty() {
                    label.to_string()
                } else if ax_hidden(t) {
                    raw_text(doc, target)
                } else {
                    visible_text(doc, target)
                };
                let text = collapse_ws(&text);
                if !text.is_empty() {
                    parts.push(text);
                }
            }
        }
        let joined = parts.join(" ");
        if !joined.is_empty() {
            return truncate_chars(&joined, 120);
        }
    }
    if let Some(label) = node.attr("aria-label") {
        let label = collapse_ws(label);
        if !label.is_empty() {
            return truncate_chars(&label, 120);
        }
    }
    let tag = node.tag.as_deref().unwrap_or("");
    if matches!(tag, "img" | "area") {
        let alt = collapse_ws(node.attr("alt").unwrap_or(""));
        if !alt.is_empty() {
            return truncate_chars(&alt, 120);
        }
    }
    if matches!(tag, "input" | "select" | "textarea") {
        let label_idx = node
            .attr("id")
            .and_then(|id| labels.get(id).copied())
            .or_else(|| {
                let mut cur = node.parent;
                while let Some(p) = cur {
                    if doc.nodes[p].tag.as_deref() == Some("label") {
                        return Some(p);
                    }
                    cur = doc.nodes[p].parent;
                }
                None
            });
        if let Some(label) = label_idx {
            let text = collapse_ws(&visible_text(doc, label));
            if !text.is_empty() {
                return truncate_chars(&text, 120);
            }
        }
        if tag == "input"
            && matches!(
                node.attr("type").unwrap_or("text"),
                "submit" | "button" | "reset" | "image"
            )
        {
            let value = collapse_ws(node.attr("value").unwrap_or(""));
            if !value.is_empty() {
                return truncate_chars(&value, 120);
            }
        }
        let placeholder = collapse_ws(node.attr("placeholder").unwrap_or(""));
        if !placeholder.is_empty() {
            return truncate_chars(&placeholder, 120);
        }
    }
    if matches!(tag, "table" | "fieldset") {
        let want = if tag == "table" { "caption" } else { "legend" };
        if let Some(&child) = node
            .children
            .iter()
            .find(|&&c| doc.nodes[c].tag.as_deref() == Some(want))
        {
            let text = collapse_ws(&visible_text(doc, child));
            if !text.is_empty() {
                return truncate_chars(&text, 120);
            }
        }
    }
    if name_from_content(role) {
        let text = collapse_ws(&visible_text(doc, idx));
        if !text.is_empty() {
            return truncate_chars(&text, 120);
        }
    }
    truncate_chars(&collapse_ws(node.attr("title").unwrap_or("")), 120)
}

fn truncate_chars(s: &str, max: usize) -> String {
    let t = s.trim();
    match t.char_indices().nth(max) {
        Some((byte_idx, _)) => t[..byte_idx].to_string(),
        None => t.to_string(),
    }
}

#[pyclass(unsendable, weakref)]
struct Doc {
    doc: Rc<RefCell<dom::Document>>,
    ggjs: Option<jsvm::page::PageVm>,
}

#[pyfunction]
fn parse_html(html: &str) -> Doc {
    Doc {
        doc: Rc::new(RefCell::new(html::parse(html))),
        ggjs: None,
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

type ScriptRecord = (usize, String, String, String);

/// Connected script elements in document order. Keeping this in the native
/// DOM makes dynamically appended nodes and HTMLScriptElement property writes
/// visible to the host loader without rebuilding the Python tree.
fn collect_script_records(doc: &dom::Document) -> Vec<ScriptRecord> {
    let mut out = Vec::new();
    let mut stack = vec![doc.root];
    while let Some(idx) = stack.pop() {
        let node = &doc.nodes[idx];
        if node.tag.as_deref() == Some("script") {
            let stype = node.attr("type").unwrap_or("").trim().to_lowercase();
            let is_module = stype == "module";
            let is_javascript = stype.is_empty()
                || is_module
                || stype.contains("javascript")
                || stype.contains("ecmascript");
            if is_javascript {
                let (kind, value) = if let Some(src) = node.attr("src") {
                    ("src".to_string(), src.to_string())
                } else {
                    ("inline".to_string(), doc.collect_text(idx))
                };
                if kind == "src" || !value.trim().is_empty() {
                    let mode = if kind == "inline" {
                        // async/defer do not affect classic inline scripts.
                        if is_module { "module" } else { "blocking" }
                    } else if doc.script_async_overrides.get(&idx)
                        == Some(&false)
                    {
                        // Dynamically inserted script with async explicitly
                        // disabled: preserve insertion order.
                        if is_module { "ordered-module" } else { "ordered" }
                    } else if doc.script_created_dynamically.contains(&idx)
                        || node.attr("async").is_some()
                        || doc.script_async_overrides.get(&idx) == Some(&true)
                    {
                        if is_module { "async-module" } else { "async" }
                    } else if is_module {
                        "module"
                    } else if node.attr("defer").is_some() {
                        "defer"
                    } else {
                        "blocking"
                    };
                    out.push((idx, kind, value, mode.to_string()));
                }
            }
        }
        for &child in node.children.iter().rev() {
            stack.push(child);
        }
    }
    out
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

    /// (node index, kind, source/code, scheduling mode) in document order.
    fn script_records(&self) -> Vec<ScriptRecord> {
        collect_script_records(&self.doc.borrow())
    }

    /// Inline <script type="importmap"> JSON bodies in document order.
    fn import_map_sources(&self) -> Vec<String> {
        let doc = self.doc.borrow();
        let mut out = Vec::new();
        let mut stack = vec![doc.root];
        while let Some(idx) = stack.pop() {
            let node = &doc.nodes[idx];
            if node.tag.as_deref() == Some("script")
                && node.attr("type").unwrap_or("").trim()
                    .eq_ignore_ascii_case("importmap")
            {
                let source = doc.collect_text(idx);
                if !source.trim().is_empty() {
                    out.push(source);
                }
            }
            for &child in node.children.iter().rev() {
                stack.push(child);
            }
        }
        out
    }

    /// Compatibility execution view used by older Python loaders.
    fn script_entries(&self) -> Vec<(String, String)> {
        let doc = self.doc.borrow();
        let mut immediate = Vec::new();
        let mut deferred = Vec::new();
        for (_, kind, value, mode) in collect_script_records(&doc) {
            let entry = (kind, value);
            if mode == "defer" || mode == "module" {
                deferred.push(entry);
            } else {
                immediate.push(entry);
            }
        }
        immediate.extend(deferred);
        immediate
    }

    /// ("inline", code) and ("src", url) entries in EXECUTION order:
    /// parser-order scripts first, then `defer` scripts (and modules,
    /// which defer per spec) in document order — Naver's app bundles
    /// are all defer and read inline-defined globals (EAGER-DATA.GV)
    /// that appear later in the document.
    fn script_entries_legacy(&self) -> Vec<(String, String)> {
        let doc = self.doc.borrow();
        let mut out = Vec::new();
        let mut deferred = Vec::new();
        let mut stack = vec![doc.root];
        while let Some(idx) = stack.pop() {
            let node = &doc.nodes[idx];
            if node.tag.as_deref() == Some("script") {
                let stype = node.attr("type").unwrap_or("").to_lowercase();
                if stype.is_empty()
                    || stype.contains("javascript")
                    || stype == "module"
                {
                    let is_defer = node.attr("defer").is_some()
                        || stype == "module";
                    let entry = if let Some(src) = node.attr("src") {
                        Some(("src".to_string(), src.to_string()))
                    } else {
                        let code: String = node
                            .children
                            .iter()
                            .map(|&c| doc.nodes[c].text.as_str())
                            .collect();
                        if code.trim().is_empty() {
                            None
                        } else {
                            Some(("inline".to_string(), code))
                        }
                    };
                    if let Some(e) = entry {
                        // defer without src is ignored per spec —
                        // inline scripts always run in parser order
                        if is_defer && e.0 == "src" {
                            deferred.push(e);
                        } else {
                            out.push(e);
                        }
                    }
                }
            }
            for &c in node.children.iter().rev() {
                stack.push(c);
            }
        }
        out.extend(deferred);
        out
    }

    /// Partial invalidation: recompute styles and report the damage
    /// class instead of making Python rebuild everything.
    /// 0 = nothing changed; 1 = paint-only (per-node style patches
    /// returned); 2 = a geometry property changed (relayout);
    /// 3 = structure changed (pseudo nodes appeared/vanished — the
    /// caller must re-export the tree).
    #[pyo3(signature = (css_sources, viewport_width=1280.0))]
    fn restyle_diff(
        &mut self,
        css_sources: Vec<String>,
        viewport_width: f64,
    ) -> (u8, Vec<(usize, Vec<(String, String)>)>) {
        let mut d = self.doc.borrow_mut();
        let n_before = d.nodes.len();
        let old: Vec<std::collections::HashMap<String, String>> =
            d.nodes.iter().map(|nd| nd.style.clone()).collect();
        style::compute_styles_vw(&mut d, &css_sources, viewport_width);
        if d.nodes.len() != n_before {
            return (3, Vec::new());
        }
        let mut patches = Vec::new();
        let mut geometry = false;
        for (i, nd) in d.nodes.iter().enumerate() {
            if nd.style == old[i] {
                continue;
            }
            for (k, v) in nd.style.iter() {
                if old[i].get(k) != Some(v) && !is_paint_only_prop(k) {
                    geometry = true;
                }
            }
            for k in old[i].keys() {
                if !nd.style.contains_key(k) && !is_paint_only_prop(k)
                {
                    geometry = true;
                }
            }
            patches.push((
                i,
                nd.style
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            ));
        }
        if patches.is_empty() {
            return (0, Vec::new());
        }
        if geometry {
            return (2, Vec::new());
        }
        (1, patches)
    }

    /// Push layout results so page JS reading getBoundingClientRect
    /// gets real geometry (document coordinates). Call after layout.
    fn set_layout_rects(
        &mut self,
        rects: Vec<(u32, f64, f64, f64, f64)>,
    ) {
        self.ggvm().set_layout_rects(rects);
    }

    /// Push scroll-container state (node, scrollTop, scrollLeft,
    /// scrollHeight, scrollWidth) so page JS observes real scrolling.
    fn set_scroll_state(
        &mut self,
        state: Vec<(u32, f64, f64, f64, f64)>,
    ) {
        self.ggvm().set_scroll_state(state);
    }

    /// Drain `el.scrollTop = n` writes page scripts made this turn:
    /// [(node, scrollTop, scrollLeft)] for the host to apply.
    fn take_scroll_writes(&mut self) -> Vec<(u32, f64, f64)> {
        self.ggvm().take_scroll_writes()
    }

    /// Shell-side hover update: mark the element under the pointer
    /// and its ancestors so `:hover` rules match on the next
    /// compute_styles. Pass None when the pointer leaves the page.
    #[pyo3(signature = (node_idx=None))]
    fn set_hover(&mut self, node_idx: Option<usize>) {
        let mut d = self.doc.borrow_mut();
        d.hover_chain.clear();
        let mut cur = node_idx.filter(|&i| i < d.nodes.len());
        while let Some(i) = cur {
            d.hover_chain.push(i);
            cur = d.nodes[i].parent;
        }
    }

    /// Shell-side focus update for `:focus` rules.
    #[pyo3(signature = (node_idx=None))]
    fn set_focus(&mut self, node_idx: Option<usize>) {
        let mut d = self.doc.borrow_mut();
        d.focused = node_idx.filter(|&i| i < d.nodes.len());
    }

    /// Shell-side attribute write (typed input values): mirror the
    /// change into the DOM so page JS reads what the user typed.
    fn set_attr(&mut self, node_idx: usize, name: String, value: String) {
        let mut d = self.doc.borrow_mut();
        if node_idx < d.nodes.len() {
            d.set_attr(node_idx, &name, &value);
        }
    }

    /// Shell-side attribute removal (form reset and boolean state).
    fn remove_attr(&mut self, node_idx: usize, name: String) {
        let mut d = self.doc.borrow_mut();
        if node_idx < d.nodes.len() {
            d.remove_attr(node_idx, &name);
        }
    }

    /// Tell gg-js the page URL so `location.*` is real.
    /// Call before run_scripts.
    fn set_page_url(&mut self, url: String) {
        self.ggvm().set_page_url(&url);
    }

    /// Seed document.cookie from the network jar before scripts run.
    fn seed_cookies(&mut self, cookies: String) {
        if !cookies.is_empty() {
            self.ggvm().seed_cookies(&cookies);
        }
    }

    /// Read the current document.cookie-visible string.
    fn read_cookies(&self) -> String {
        self.ggjs
            .as_ref()
            .map(|vm| vm.cookies_string())
            .unwrap_or_default()
    }

    /// Drain original document.cookie setter strings so the Python network
    /// jar can validate attributes and apply them with the page URL context.
    fn take_cookie_writes(&mut self) -> Vec<String> {
        self.ggjs
            .as_mut()
            .map(|vm| vm.take_cookie_writes())
            .unwrap_or_default()
    }

    /// One real-time slice of the live event loop: fires timers/rAF
    /// due within the next dt_ms of virtual time. Returns (console
    /// output, fetches to service). The render loop calls this
    /// periodically after load.
    fn tick(&mut self, dt_ms: f64) -> (Vec<String>, Vec<(u32, String)>) {
        self.ggvm().tick(dt_ms)
    }

    fn tick_requests(&mut self, dt_ms: f64) -> (Vec<String>, Vec<(
        u32, String, String, String, Vec<(String, String)>, String, String,
    )>) {
        self.ggvm().tick_requests(dt_ms)
    }

    /// DOM mutation counter — re-style/re-layout only when it changes.
    fn dom_version(&self) -> u64 {
        self.doc.borrow().version
    }

    /// Fire DOMContentLoaded / load once all scripts have run — app
    /// bundles bootstrap from these. Returns console output.
    fn fire_lifecycle(&mut self) -> Vec<String> {
        self.ggvm().fire_lifecycle()
    }

    /// Complete parsing and dispatch DOMContentLoaded without waiting for
    /// outstanding async or dynamically inserted external scripts.
    fn fire_dom_content_loaded(&mut self) -> Vec<String> {
        self.ggvm().fire_dom_content_loaded()
    }

    /// Mark the document complete and dispatch the window/document load
    /// events after the host loader has settled all load-blocking scripts.
    fn fire_load(&mut self) -> Vec<String> {
        self.ggvm().fire_load()
    }

    /// (listeners, timers, microtasks) for boot diagnosis.
    fn pending_counts(&mut self) -> (usize, usize, usize) {
        self.ggvm().pending_counts()
    }

    /// Run scripts (in order) against the DOM. Returns console output.
    /// The JS context persists, so later events see earlier definitions.
    fn run_scripts(&mut self, sources: Vec<String>) -> Vec<String> {
        self.ggvm().run_scripts(&sources)
    }

    /// Bubble a click through onclick attributes + addEventListener
    /// handlers. Returns (console output, whether any handler ran,
    /// whether the default action was prevented).
    fn dispatch_click(
        &mut self,
        node_idx: usize,
    ) -> (Vec<String>, bool, bool) {
        self.ggvm().dispatch_click(node_idx)
    }

    /// Dispatch a host-initiated DOM event and report cancellation.
    #[pyo3(signature = (
        node_idx,
        event_type,
        bubbles=true,
        cancelable=true,
        submitter_idx=None
    ))]
    fn dispatch_event(
        &mut self,
        node_idx: usize,
        event_type: String,
        bubbles: bool,
        cancelable: bool,
        submitter_idx: Option<usize>,
    ) -> (Vec<String>, bool, bool) {
        self.ggvm().dispatch_event(
            node_idx,
            &event_type,
            bubbles,
            cancelable,
            submitter_idx,
        )
    }

    /// Run the gg-js event loop to a fixed point. Returns
    /// (console output, [(fetch_id, url)] to service).
    fn pump(&mut self) -> (Vec<String>, Vec<(u32, String)>) {
        self.ggvm().pump()
    }

    fn pump_requests(&mut self) -> (Vec<String>, Vec<(
        u32, String, String, String, Vec<(String, String)>, String, String,
    )>) {
        self.ggvm().pump_requests()
    }

    /// Drain a Promise microtask checkpoint without advancing timers.
    fn pump_microtasks_requests(&mut self) -> (Vec<String>, Vec<(
        u32, String, String, String, Vec<(String, String)>, String, String,
    )>) {
        self.ggvm().pump_microtasks_requests()
    }

    /// Opt-in GG_JS_PROFILE samples since the previous call.
    fn take_js_profile(&mut self) -> Vec<(String, u32, u32, u64)> {
        self.ggvm().take_profile()
    }

    /// Read a numeric script global without evaluating another program.
    fn global_number(&mut self, name: String) -> Option<f64> {
        self.ggvm().global_number(&name)
    }

    /// Current virtual-clock ms (host fixes a settle horizon from this).
    fn now_ms(&mut self) -> f64 {
        self.ggvm().now_ms()
    }

    /// Fire one scheduler slice; host refreshes layout rects between
    /// steps so geometry-reading effects see real rects. Returns
    /// (console output, fetches, more-work-remains).
    fn step(&mut self, horizon_ms: f64) -> (Vec<String>, Vec<(u32, String)>, bool) {
        self.ggvm().step(horizon_ms)
    }

    /// Host settles a fetch the driver performed (gg-js only).
    fn resolve_fetch(&mut self, fetch_id: u32, status: u16, body: String) {
        self.ggvm().resolve_fetch(fetch_id, status, body);
    }

    /// `headers` is optional so callers built against the older 4-arg
    /// signature keep working (they just get a header-less Response).
    #[pyo3(signature = (fetch_id, status, url, body, headers=None))]
    fn resolve_fetch_full(
        &mut self,
        fetch_id: u32,
        status: u16,
        url: String,
        body: String,
        headers: Option<Vec<(String, String)>>,
    ) {
        self.ggvm().resolve_fetch_full(
            fetch_id, status, url, body, headers.unwrap_or_default());
    }

    fn reject_fetch(&mut self, fetch_id: u32, message: String) {
        self.ggvm().reject_fetch(fetch_id, message);
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
        // id -> node and label[for] -> label maps for name computation
        let mut ids = std::collections::HashMap::new();
        let mut labels = std::collections::HashMap::new();
        for (i, node) in doc.nodes.iter().enumerate() {
            if !node.is_element() {
                continue;
            }
            if let Some(id) = node.attr("id") {
                ids.entry(id.to_string()).or_insert(i);
            }
            if node.tag.as_deref() == Some("label") {
                if let Some(target) = node.attr("for") {
                    labels.entry(target.to_string()).or_insert(i);
                }
            }
        }
        let mut out = Vec::new();
        let mut stack = vec![doc.root];
        while let Some(idx) = stack.pop() {
            let node = &doc.nodes[idx];
            if let Some(tag) = node.tag.as_deref() {
                // hidden subtrees leave the accessibility projection
                if idx != doc.root && ax_hidden(node) {
                    continue;
                }
                if let Some(role) = implicit_role(&doc, idx) {
                    let mut name =
                        accessible_name(&doc, idx, &role, &ids, &labels);
                    if name.is_empty() {
                        // keep the old subtree-text behavior for
                        // containers so agent consumers still see
                        // something useful
                        name = truncate_chars(&visible_text(&doc, idx), 120);
                    }
                    out.push((
                        idx as u64,
                        role,
                        tag.to_string(),
                        name,
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

#[cfg(test)]
mod display_list_tests {
    use super::{Cmd, TextEngine};

    fn marker(kind: u8, normal: f64, maximum: f64, inset: f64) -> Cmd {
        (kind, 0.0, normal, 0.0, maximum, (0, 0, 0), inset, 0, String::new())
    }

    fn rect(top: f64, bottom: f64) -> Cmd {
        (0, 0.0, top, 20.0, bottom, (1, 2, 3), 0.0, 0, String::new())
    }

    #[test]
    fn sticky_groups_resolve_from_scroll_without_rebuilding_the_list() {
        let list = vec![
            marker(8, 100.0, 300.0, 10.0),
            rect(100.0, 200.0),
            marker(9, 0.0, 0.0, 0.0),
        ];
        let before = TextEngine::viewport_cmds(&list, 0.0, 0.0, 500.0, 200.0);
        assert_eq!(before.len(), 1);
        assert_eq!((before[0].2, before[0].4), (100.0, 200.0));

        let stuck = TextEngine::viewport_cmds(&list, 0.0, 150.0, 500.0, 200.0);
        assert_eq!((stuck[0].2, stuck[0].4), (10.0, 110.0));

        // Once the containing-block boundary is reached, the item scrolls
        // away instead of remaining pinned beyond its parent.
        let bounded =
            TextEngine::viewport_cmds(&list, 0.0, 350.0, 500.0, 200.0);
        assert_eq!((bounded[0].2, bounded[0].4), (-50.0, 50.0));
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
