"""Native text engine: Rust-side font measurement + rasterization.

When available, layout measures text through Rust (no Tcl round-trips)
and the whole page is rasterized in Rust; tkinter only displays the
resulting image. Falls back to tkinter fonts/canvas when missing.
"""

try:
    import ggcore
    _engine = ggcore.TextEngine() if hasattr(ggcore, "TextEngine") else None
except Exception:
    _engine = None


def available():
    return _engine is not None


def engine():
    return _engine


def has_family(name):
    """Is this font family resolvable (built-in table or a
    registered @font-face web font)?"""
    return _engine is not None and _engine.has_family(name)


def load_svgs(nodes):
    """Rasterize every inline <svg> to an image handle (node._img),
    so layout/paint treat it exactly like an <img>. Fill resolution:
    computed style `fill` (inline style attr included via the cascade)
    beats the presentation attribute; the svg element's fill inherits
    to paths; default is black. `fill: none` paths are skipped."""
    if _engine is None:
        return
    from .colors import to_rgb
    from .html_parser import Element, tree_to_list

    def fill_of(path_node, svg_node):
        for cand in (path_node.style.get("fill"),
                     path_node.attributes.get("fill"),
                     svg_node.style.get("fill"),
                     svg_node.attributes.get("fill")):
            if cand and cand.strip():
                c = cand.strip()
                if c == "currentColor":
                    c = svg_node.style.get("color", "black")
                return None if c == "none" else to_rgb(c)
        return to_rgb("black")

    for svg in tree_to_list(nodes, []):
        if not (isinstance(svg, Element) and svg.tag == "svg"):
            continue
        paths = []
        for n in tree_to_list(svg, []):
            if isinstance(n, Element) and n.tag == "path" \
                    and n.attributes.get("d"):
                rgb = fill_of(n, svg)
                if rgb is not None:
                    paths.append((n.attributes["d"], rgb))
        vb = (svg.attributes.get("viewbox")
              or svg.attributes.get("viewBox") or "").split()
        try:
            vx, vy, vw, vh = (float(v) for v in vb)
        except ValueError:
            try:
                vw = float(svg.attributes.get("width", ""))
                vh = float(svg.attributes.get("height", ""))
            except ValueError:
                vw, vh = 24.0, 24.0
            vx, vy = 0.0, 0.0
        if vw <= 0 or vh <= 0 or not paths:
            svg._img = None
            continue
        out_w = max(1, round(vw))
        out_h = max(1, round(vh))
        svg._img = _engine.load_svg(
            (vx, vy, vw, vh), out_w, out_h, paths)


def load_canvases(nodes, doc):
    """T4: bake each canvas's recorded 2D commands into an image
    handle (node._img) via the Rust rasterizer. Path fills beyond
    rect/full-circle draw as outlines (v1 gap)."""
    if _engine is None or doc is None \
            or not hasattr(doc, "canvas_nodes"):
        return
    from math import pi
    from .colors import to_rgb
    from .html_parser import Element, tree_to_list
    active = set(doc.canvas_nodes())
    if not active:
        return
    default_font = _engine.font_id("default", False, False)
    for node in tree_to_list(nodes, []):
        if not (isinstance(node, Element) and node.tag == "canvas"):
            continue
        ridx = getattr(node, "_ridx", None)
        if ridx is None or ridx not in active:
            continue
        try:
            w = int(float(node.attributes.get("width", 300) or 300))
            h = int(float(node.attributes.get("height", 150) or 150))
        except ValueError:
            w, h = 300, 150
        cmds = []
        path = []          # [(x, y)] current subpath
        arcs = []          # [(x, y, r, full)] arcs in current path
        for (op, a, b, c, d, e, text, style) in doc.canvas_cmds(ridx):
            rgb = to_rgb(style or "black")
            if op == 1:      # fillRect
                cmds.append((0, a, b, a + c, b + d, rgb, 0.0, 0, ""))
            elif op == 2:    # strokeRect
                for (x1, y1, x2, y2) in ((a, b, a + c, b),
                                         (a, b + d, a + c, b + d),
                                         (a, b, a, b + d),
                                         (a + c, b, a + c, b + d)):
                    cmds.append((2, x1, y1, x2, y2, rgb, 1.0, 0, ""))
            elif op == 3:    # clearRect (partial — full clears reset
                cmds.append((0, a, b, a + c, b + d,   # in the VM)
                             (255, 255, 255), 0.0, 0, ""))
            elif op == 4:    # fillText(text, x, y-baseline)
                px = e or 10.0
                cmds.append((1, a, b - px, 0.0, 0.0, rgb, px,
                             default_font, text))
            elif op == 5:    # beginPath
                path, arcs = [], []
            elif op == 6:    # moveTo
                path.append(("m", a, b))
            elif op == 7:    # lineTo
                path.append(("l", a, b))
            elif op == 8:    # arc(x, y, r, a0, a1)
                arcs.append((a, b, c, abs(e - d) >= 2 * pi - 1e-3))
            elif op == 9:    # rect(x, y, w, h): closed subpath
                path.extend((("m", a, b), ("l", a + c, b),
                             ("l", a + c, b + d), ("l", a, b + d),
                             ("l", a, b), ("m", a, b)))
            elif op == 10:   # closePath
                first = next(((x, y) for (k, x, y) in path
                              if k == "m"), None)
                if first:
                    path.append(("l", first[0], first[1]))
            elif op in (11, 12):  # fill / stroke
                for (cx, cy, r, full) in arcs:
                    if full:
                        cmds.append((3, cx - r, cy - r, cx + r,
                                     cy + r, rgb, 0.0, 0, ""))
                prev = None
                for (k, x, y) in path:
                    if k == "l" and prev is not None:
                        cmds.append((2, prev[0], prev[1], x, y,
                                     rgb, 1.0, 0, ""))
                    prev = (x, y)
        if cmds:
            node._img = _engine.load_canvas(w, h, cmds)


def load_background_images(nodes, fetch_raw):
    """Decode every element's first CSS background-image layer.
    fetch_raw(urls) -> {url: bytes} does the (parallel) networking.
    Sets node._bg = (image_id, w, h, spec) for layout/paint."""
    if _engine is None:
        return
    from .draw import parse_background
    from .html_parser import Element, tree_to_list

    jobs = []
    for n in tree_to_list(nodes, []):
        if isinstance(n, Element):
            n._bg = None
            spec = parse_background(n.style)
            if spec:
                jobs.append((n, spec))
    if not jobs:
        return
    urls = list({spec["url"] for _n, spec in jobs})
    raw = fetch_raw(urls)
    handles = {}
    for u in urls:
        data = raw.get(u)
        try:
            handles[u] = _engine.load_image(data) if data else None
        except Exception:
            handles[u] = None
    for n, spec in jobs:
        img = handles.get(spec["url"])
        if img:
            n._bg = (img[0], img[1], img[2], spec)


class NativeFont:
    """Same interface the layout code expects from a cached tk font."""

    __slots__ = ("id", "size", "gg_ascent", "gg_descent", "gg_linespace",
                 "gg_widths")

    def __init__(self, size, weight, slant, family):
        self.id = _engine.font_id(
            family, weight == "bold", slant == "italic")
        self.size = size
        ascent, descent, linespace = _engine.metrics(self.id, float(size))
        self.gg_ascent = ascent
        self.gg_descent = descent
        self.gg_linespace = linespace
        self.gg_widths = {}

    def measure(self, text):
        return _engine.measure(self.id, float(self.size), text)

    def metrics(self, key):
        return {
            "ascent": self.gg_ascent,
            "descent": self.gg_descent,
            "linespace": self.gg_linespace,
        }[key]
