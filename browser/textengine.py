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
_bg_vector_cache = {}


def clear_image_caches():
    """Drop cached SVG/background handles. Must accompany every
    engine.clear_images() — the ids outlive nothing."""
    _svg_cache.clear()
    _bg_cache.clear()
    _bg_vector_cache.clear()


def rasterize_svg(svg, out_w=None, out_h=None):
    """Rasterize one <svg> element to an image handle, or None.

    `out_w`/`out_h` override the intrinsic size with the used size CSS
    decided for the image. That matters for more than resolution: a
    file with no viewBox draws in its viewport's own coordinate
    system, so `width="100%"` on a shape inside it means 100% of the
    box it was placed in, and the same file is a different picture at
    a different size.

    Fill resolution: computed style `fill` (inline style attr included
    via the cascade) beats the presentation attribute; the svg
    element's fill inherits to shapes; default is black. `fill: none`
    shapes are skipped.

    The native rasterizer consumes path data, so basic circle and rect
    elements are lowered to equivalent paths before crossing the binding.
    """
    if _engine is None:
        return None
    from .colors import to_rgb
    from .html_parser import Element, tree_to_list

    def style_of(node):
        return getattr(node, "style", None) or {}

    def fill_of(path_node, svg_node):
        for cand in (style_of(path_node).get("fill"),
                     path_node.attributes.get("fill"),
                     style_of(svg_node).get("fill"),
                     svg_node.attributes.get("fill")):
            if cand and cand.strip():
                c = cand.strip()
                if c == "currentColor":
                    c = style_of(svg_node).get("color", "black")
                return None if c == "none" else to_rgb(c)
        return to_rgb("black")

    # Geometry is expressed in the viewBox's coordinate system, and a
    # percentage there resolves against the viewport the viewBox sets
    # up — so the box has to be known before any shape is lowered.
    vx, vy, vw, vh, out_w, out_h = _svg_viewport(svg, out_w, out_h)
    if vw <= 0 or vh <= 0:
        return None

    def number(node, name, basis, default=0.0):
        raw = node.attributes.get(name)
        if raw is None:
            return default
        raw = raw.strip()
        try:
            if raw.endswith("%"):
                return float(raw[:-1]) * basis / 100.0
            return _length(raw)
        except ValueError:
            return default

    def shape_path(node):
        if node.tag == "path":
            return node.attributes.get("d") or None
        if node.tag == "rect":
            x, y = number(node, "x", vw), number(node, "y", vh)
            width = number(node, "width", vw)
            height = number(node, "height", vh)
            if width <= 0 or height <= 0:
                return None
            return (f"M{x + vx} {y + vy}H{x + vx + width}"
                    f"V{y + vy + height}H{x + vx}Z")
        if node.tag == "circle":
            diag = (vw * vw + vh * vh) ** 0.5 / (2 ** 0.5)
            cx, cy = number(node, "cx", vw), number(node, "cy", vh)
            radius = number(node, "r", diag)
            if radius <= 0:
                return None
            return (f"M{cx - radius} {cy}"
                    f"A{radius} {radius} 0 1 0 {cx + radius} {cy}"
                    f"A{radius} {radius} 0 1 0 {cx - radius} {cy}Z")
        return None

    paths = []
    for n in tree_to_list(svg, []):
        path = shape_path(n) if isinstance(n, Element) else None
        if path:
            rgb = fill_of(n, svg)
            if rgb is not None:
                paths.append((path, rgb))
    if not paths:
        return None
    # A vector image has no resolution of its own, but this store holds
    # rasters, so it gets rasterized once and scaled by the image
    # sampler afterwards. Rasterizing at the intrinsic size and scaling
    # up is what made a two-colour SVG background come out with a
    # blended band across its colour boundary. Rasterizing larger and
    # letting the sampler come *down* keeps the edge: the supersample
    # is the closest this can get to re-rasterizing at the used size,
    # which is what the intrinsic size being a lie about the pixels
    # buys us — the caller is told the CSS size either way.
    ss = _supersample(out_w, out_h)
    key = (vx, vy, vw, vh, out_w * ss, out_h * ss,
           tuple((d, tuple(rgb)) for d, rgb in paths))
    handle = _svg_cache.get(key)
    if handle is None:
        handle = _engine.load_svg(
            (vx, vy, vw, vh), out_w * ss, out_h * ss, paths)
        _svg_cache[key] = handle
    # report the CSS size; the sampler reads the raster's real one
    return (handle[0], out_w, out_h)


# The rasterizer caps a side at 512, and a supersampled icon is pure
# memory in the image store, so the factor shrinks as the image grows.
_SS_LIMIT = 512


def _supersample(w, h):
    for factor in (4, 2):
        if w * factor <= _SS_LIMIT and h * factor <= _SS_LIMIT:
            return factor
    return 1


def _length(text):
    """Bare user units out of an SVG length ("100", "100px", "1em").
    Percentages have no basis here, so they raise like any other
    non-number and the caller decides what to do."""
    text = (text or "").strip().lower()
    for unit, scale in (("px", 1.0), ("pt", 4 / 3), ("pc", 16.0),
                        ("mm", 96 / 25.4), ("cm", 96 / 2.54),
                        ("in", 96.0), ("em", 16.0), ("ex", 8.0)):
        if text.endswith(unit):
            return float(text[:-len(unit)].strip()) * scale
    return float(text)


def _view_box(svg):
    """(vx, vy, vw, vh) of an <svg>'s viewBox, or None."""
    vb = (svg.attributes.get("viewbox")
          or svg.attributes.get("viewBox") or "").replace(",", " ").split()
    try:
        vx, vy, vw, vh = (float(v) for v in vb)
    except ValueError:
        return None
    return (vx, vy, vw, vh) if vw > 0 and vh > 0 else None


def _attr_length(svg, name):
    """An <svg> sizing attribute as absolute px, or 0 when it gives no
    absolute length — missing, unparsable, or a percentage."""
    try:
        return max(_length(svg.attributes.get(name, "")), 0.0)
    except ValueError:
        return 0.0


def svg_intrinsic(svg):
    """(width, height, ratio) an <svg> contributes to CSS sizing, with
    None for each piece it does not have.

    CSS Images 3 §5.1: an image's intrinsic dimensions are the ones it
    carries on its own. A percentage width resolves against the
    viewport the *referencing* context supplies, so it is not one of
    them and reads as absent here — which is what makes
    `background-size: contain` on such a file fill the whole box
    rather than fit a 24x24 guess into it. A viewBox contributes a
    ratio whether or not there are dimensions.
    """
    iw = _attr_length(svg, "width") or None
    ih = _attr_length(svg, "height") or None
    vb = _view_box(svg)
    if iw and ih:
        ratio = iw / ih
    elif vb:
        ratio = vb[2] / vb[3]
    else:
        ratio = None
    return iw, ih, ratio


def _svg_viewport(svg, out_w=None, out_h=None):
    """(vx, vy, vw, vh, out_w, out_h) for an <svg> element.

    The viewBox gives the coordinate system the shapes are drawn in;
    width/height give the intrinsic size, which is what the CSS sizing
    algorithms scale from and what `background-size: auto` reads its
    aspect ratio out of. They are frequently different — a 4x64
    viewBox in an 8px-by-32px image is a legitimate 1:4 picture, and
    rasterizing at the viewBox size instead would claim 1:16.

    With no viewBox there is no separate coordinate system: the
    viewport *is* user space, so the used size doubles as the basis
    every percentage inside the file resolves against.
    """
    vb = _view_box(svg)
    iw = _attr_length(svg, "width")
    ih = _attr_length(svg, "height")
    used_w = out_w if out_w else (iw or (vb[2] if vb else 24.0))
    used_h = out_h if out_h else (ih or (vb[3] if vb else 24.0))
    if vb:
        vx, vy, vw, vh = vb
    else:
        vx, vy, vw, vh = 0.0, 0.0, used_w, used_h
    return vx, vy, vw, vh, max(1, round(used_w)), max(1, round(used_h))


def load_svgs(nodes):
    """Rasterize every inline <svg> in the tree to node._img, so
    layout and paint treat it exactly like an <img>."""
    if _engine is None:
        return
    from .html_parser import Element, tree_to_list

    for svg in tree_to_list(nodes, []):
        if isinstance(svg, Element) and svg.tag == "svg":
            svg._img = rasterize_svg(svg)


def looks_like_svg(data):
    """Is this byte string an SVG document? Content sniffing, because
    the file extension is not always there and never trustworthy."""
    if not data:
        return False
    head = data[:512].lstrip()
    if head.startswith(b"<?xml") or head.startswith(b"<!--"):
        head = data[:2048]
    return b"<svg" in head[:2048].lower()


def svg_document(data):
    """The root <svg> element of an SVG file, or None.

    Kept unrasterized so the caller can decide the used size first: a
    file with no intrinsic dimensions has no picture until CSS says
    how big it is.
    """
    if not data or not looks_like_svg(data):
        return None
    from .html_parser import Element, HTMLParser, tree_to_list

    root = HTMLParser(data.decode("utf-8", errors="replace")).parse()
    for node in tree_to_list(root, []):
        if isinstance(node, Element) and node.tag == "svg":
            return node
    return None


def load_image_data(data):
    """Decode image bytes to a (id, w, h) handle, SVG included.

    The raster decoder is the `image` crate, which has no SVG support,
    so an SVG file reaching it fails and the element draws nothing.
    Route those through the same path lowering that inline <svg> uses:
    parse the markup, rasterize at the viewBox's own size (that is the
    intrinsic size the CSS sizing algorithms then scale)."""
    if _engine is None or not data:
        return None
    if not looks_like_svg(data):
        return _engine.load_image(data)
    from .html_parser import Element, HTMLParser, tree_to_list

    text = data.decode("utf-8", errors="replace")
    root = HTMLParser(text).parse()
    for node in tree_to_list(root, []):
        if isinstance(node, Element) and node.tag == "svg":
            return rasterize_svg(node)
    return None


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
            _bg_cache[u] = load_image_data(data)
            # A vector stays available unrasterized: background-size
            # may have to decide the used size before there is any
            # picture to sample (CSS Images 3 §5.3).
            _bg_vector_cache[u] = svg_document(data)
        except Exception:
            _bg_cache[u] = None
            _bg_vector_cache[u] = None
    for n, spec in jobs:
        img = _bg_cache.get(spec["url"])
        n._bg_vector = _bg_vector_cache.get(spec["url"])
        if img:
            n._bg = (img[0], img[1], img[2], spec)
        elif n._bg_vector is not None:
            n._bg = (0, 0, 0, spec)


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

    def scaled(self, factor):
        """The same face at `factor` times the size. A scaled glyph run
        is what an axis-aligned CSS transform does to text, and an
        outline font renders it identically either way."""
        if factor == 1.0 or factor <= 0:
            return self
        clone = NativeFont.__new__(NativeFont)
        clone.id = self.id
        clone.size = self.size * factor
        ascent, descent, linespace = _engine.metrics(
            self.id, float(clone.size))
        clone.gg_ascent = ascent
        clone.gg_descent = descent
        clone.gg_linespace = linespace
        clone.gg_widths = {}
        return clone

    def measure(self, text):
        return _engine.measure(self.id, float(self.size), text)

    def metrics(self, key):
        return {
            "ascent": self.gg_ascent,
            "descent": self.gg_descent,
            "linespace": self.gg_linespace,
        }[key]
