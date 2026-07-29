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


# Handles for rasterized SVGs and decoded background images, keyed by
# whatever determines the result. The engine's image store hands out a
# fresh id per call and never evicts, and these two loaders re-ran for
# every element on every DOM mutation — naver mutates on nearly every
# tick, so the store grew without bound (hundreds of MB in a minute).
# `<img>` was already covered by browser/shell's _img_by_src; these two
# had no cache at all. Cleared with the engine's store on navigation:
# the ids are only valid while it keeps them.
_svg_cache = {}
_bg_cache = {}


def clear_image_caches():
    """Drop cached SVG/background handles. Must accompany every
    engine.clear_images() — the ids outlive nothing."""
    _svg_cache.clear()
    _bg_cache.clear()


def load_svgs(nodes):
    """Rasterize every inline <svg> to an image handle (node._img),
    so layout/paint treat it exactly like an <img>. Fill resolution:
    computed style `fill` (inline style attr included via the cascade)
    beats the presentation attribute; the svg element's fill inherits
    to shapes; default is black. `fill: none` shapes are skipped.

    The native rasterizer consumes path data, so basic circle and rect
    elements are lowered to equivalent paths before crossing the binding.
    """
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

    def number(node, name, default=0.0):
        try:
            return float(node.attributes.get(name, default))
        except (TypeError, ValueError):
            return default

    def shape_path(node):
        if node.tag == "path":
            return node.attributes.get("d") or None
        if node.tag == "rect":
            x, y = number(node, "x"), number(node, "y")
            width, height = number(node, "width"), number(node, "height")
            if width <= 0 or height <= 0:
                return None
            return (f"M{x} {y}H{x + width}V{y + height}"
                    f"H{x}Z")
        if node.tag == "circle":
            cx, cy, radius = (number(node, "cx"), number(node, "cy"),
                              number(node, "r"))
            if radius <= 0:
                return None
            return (f"M{cx - radius} {cy}"
                    f"A{radius} {radius} 0 1 0 {cx + radius} {cy}"
                    f"A{radius} {radius} 0 1 0 {cx - radius} {cy}Z")
        return None

    for svg in tree_to_list(nodes, []):
        if not (isinstance(svg, Element) and svg.tag == "svg"):
            continue
        paths = []
        for n in tree_to_list(svg, []):
            if isinstance(n, Element):
                path = shape_path(n)
            else:
                path = None
            if path:
                rgb = fill_of(n, svg)
                if rgb is not None:
                    paths.append((path, rgb))
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
        # these inputs fully determine the raster, so the same icon
        # redrawn next tick reuses the handle instead of minting one
        key = (vx, vy, vw, vh, out_w, out_h,
               tuple((d, tuple(rgb)) for d, rgb in paths))
        handle = _svg_cache.get(key)
        if handle is None:
            handle = _engine.load_svg(
                (vx, vy, vw, vh), out_w, out_h, paths)
            _svg_cache[key] = handle
        svg._img = handle


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
    # only fetch and decode what is not already held: this used to
    # re-download and re-decode every background layer on every DOM
    # mutation, and each decode was a permanent entry in the store
    missing = [u for u in urls if u not in _bg_cache]
    raw = fetch_raw(missing) if missing else {}
    for u in missing:
        data = raw.get(u)
        try:
            _bg_cache[u] = _engine.load_image(data) if data else None
        except Exception:
            _bg_cache[u] = None
    for n, spec in jobs:
        img = _bg_cache.get(spec["url"])
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
