"""Layout engine: builds a box tree from the styled DOM.

DocumentLayout -> BlockLayout (block or inline mode)
              -> LineLayout -> TextLayout (one word each)
"""

import re
import tkinter.font

from . import textengine
from .colors import NAMED
from .draw import (DrawBgImage, DrawClipPop, DrawClipPush, DrawImage,
                   DrawLine, DrawOval, DrawRect, DrawText,
                   translate_cmds)
from .html_parser import Element, Text
from .style import parse_px, parse_size

HSTEP = 13
VSTEP = 18

BLOCK_ELEMENTS = {
    "html", "body", "article", "section", "nav", "aside",
    "h1", "h2", "h3", "h4", "h5", "h6", "hgroup", "header",
    "footer", "address", "p", "hr", "pre", "blockquote",
    "ol", "ul", "menu", "li", "dl", "dt", "dd", "figure",
    "figcaption", "main", "div", "table", "form", "fieldset",
    "legend", "details", "summary", "center", "tr", "td", "th",
    "thead", "tbody", "caption",
}

FONT_FAMILIES = {
    "default": "Segoe UI",
    "monospace": "Consolas",
    "serif": "Georgia",
    "sans-serif": "Segoe UI",
}

_FONT_CACHE = {}


def get_font(size, weight, slant, family="default"):
    key = (size, weight, slant, family)
    if key not in _FONT_CACHE:
        resolved = FONT_FAMILIES.get(family, family)
        if textengine.available():
            font = textengine.NativeFont(size, weight, slant, resolved)
            _FONT_CACHE[key] = (font, None)
        else:
            font = tkinter.font.Font(
                size=-size,  # negative = pixels
                weight=weight, slant=slant, family=resolved,
            )
            # px size under the same name NativeFont exposes
            # (line-height reads font.size on both paths)
            font.size = size
            # A hidden Label keeps the font object alive and fast.
            label = tkinter.Label(font=font)
            # Metrics are Tcl calls; compute once and cache on the font.
            font.gg_ascent = font.metrics("ascent")
            font.gg_descent = font.metrics("descent")
            font.gg_linespace = font.metrics("linespace")
            font.gg_widths = {}
            _FONT_CACHE[key] = (font, label)
    return _FONT_CACHE[key][0]


def measure(font, text):
    """Memoized font.measure — each unique word is measured once."""
    width = font.gg_widths.get(text)
    if width is None:
        width = font.measure(text)
        if len(font.gg_widths) < 100000:
            font.gg_widths[text] = width
    return width


def cached_font(node):
    """Font for a node's computed style, resolved once per style pass.
    (style() resets node._font when styles are recomputed.)"""
    font = getattr(node, "_font", None)
    if font is None:
        font = font_for(node.style)
        node._font = font
    return font


def font_for(style):
    weight = style.get("font-weight", "normal")
    if weight not in ("normal", "bold"):
        try:
            weight = "bold" if int(weight) >= 600 else "normal"
        except ValueError:
            weight = "normal"
    slant = "italic" if style.get("font-style") == "italic" else "roman"
    size = int(parse_px(style.get("font-size", "16px"), 16.0))
    size = max(size, 1)
    family = style.get("font-family", "default").split(",")[0].strip()
    family = family.strip("'\"").casefold()
    if family not in FONT_FAMILIES:
        known = {"consolas", "courier new", "georgia", "times new roman",
                 "arial", "verdana", "tahoma", "segoe ui", "malgun gothic"}
        if family not in known and not textengine.has_family(family):
            family = "default"  # unknown and no web font registered
    return get_font(size, weight, slant, family)


NON_COLOR_KEYWORDS = {
    "transparent", "inherit", "initial", "unset", "currentcolor",
    "none", "auto", "revert", "revert-layer",
    # background shorthand tokens that aren't colors
    "repeat", "no-repeat", "repeat-x", "repeat-y", "scroll", "fixed",
    "local", "cover", "contain", "center", "top", "bottom", "left",
    "right", "border-box", "padding-box", "content-box",
}


def _bg_length(token, box, tile):
    """Resolve one background-position component against free space."""
    t = (token or "").casefold()
    if t in ("left", "top"):
        return 0.0
    if t in ("right", "bottom"):
        return box - tile
    if t == "center":
        return (box - tile) / 2.0
    if t.endswith("%"):
        try:
            return (box - tile) * float(t[:-1]) / 100.0
        except ValueError:
            return 0.0
    v = parse_size(t)
    return v if v is not None else 0.0


def _bg_tile_size(spec, box_w, box_h, iw, ih):
    """background-size -> (tile_w, tile_h) in px."""
    size = spec["size"]
    kw = size[0].casefold() if size else "auto"
    if kw == "cover" or kw == "contain":
        if iw <= 0 or ih <= 0:
            return iw, ih
        s = (max if kw == "cover" else min)(box_w / iw, box_h / ih)
        return iw * s, ih * s
    def one(token, base, natural):
        t = (token or "auto").casefold()
        if t == "auto":
            return None
        if t.endswith("%"):
            try:
                return base * float(t[:-1]) / 100.0
            except ValueError:
                return None
        return parse_size(t)
    w = one(size[0] if size else None, box_w, iw)
    h = one(size[1] if len(size) > 1 else None, box_h, ih)
    if w is None and h is None:
        return float(iw), float(ih)
    if w is None:
        return (h * iw / ih if ih else h), h
    if h is None:
        return w, (w * ih / iw if iw else w)
    return w, h


def paint_background_image(node, x1, y1, x2, y2):
    """DrawBgImage for node's first background layer, or None."""
    bg = getattr(node, "_bg", None)
    if not bg:
        return None
    image_id, iw, ih, spec = bg
    box_w, box_h = x2 - x1, y2 - y1
    if box_w <= 0 or box_h <= 0 or iw <= 0 or ih <= 0:
        return None
    tile_w, tile_h = _bg_tile_size(spec, box_w, box_h, iw, ih)
    if tile_w < 1 or tile_h < 1:
        return None
    pos = spec["position"]
    # "bottom" alone means (center, bottom); a single non-keyword
    # value means (value, center)
    px = pos[0] if pos else "0%"
    py = pos[1] if len(pos) > 1 else (
        px if px in ("top", "bottom") else "center" if pos else "0%")
    if px in ("top", "bottom"):
        px = pos[1] if len(pos) > 1 else "center"
        py = pos[0]
    off_x = _bg_length(px, box_w, tile_w)
    off_y = _bg_length(py, box_h, tile_h)
    rep = spec["repeat"]
    rep_x = rep in ("repeat", "repeat-x")
    rep_y = rep in ("repeat", "repeat-y")
    return DrawBgImage(x1, y1, box_w, box_h, image_id,
                       off_x, off_y, tile_w, tile_h, rep_x, rep_y)


def effective_opacity(node):
    """Product of the element's opacity chain (approximation: opacity
    composites subtrees; we only need the 'effectively hidden' case)."""
    o = 1.0
    while node is not None:
        try:
            o *= max(0.0, min(1.0, float(node.style.get("opacity", 1))))
        except (ValueError, TypeError):
            pass
        node = node.parent
    return o


def line_height_factor(node, font_px):
    """CSS line-height as a multiple of the font's natural linespace.
    `normal` (default) keeps the engine's 1.25; a number multiplies the
    font size; a length is taken relative to the font size. Returns the
    factor to apply to (ascent+descent)."""
    raw = node.style.get("line-height", "").strip().casefold()
    if not raw or raw == "normal":
        return 1.25
    try:
        if raw.endswith("px"):
            target = float(raw[:-2])
        elif raw.endswith(("em", "rem")):
            target = float(raw.rstrip("erm")) * font_px
        elif raw.endswith("%"):
            target = float(raw[:-1]) / 100.0 * font_px
        else:
            target = float(raw) * font_px  # unitless multiplier
    except ValueError:
        return 1.25
    # convert the target line box height into a factor over font metrics
    return max(target / font_px, 0.1) if font_px else 1.25


def corner_radius(node, w, h):
    """First border-radius value in px (uniform corners; % of the
    smaller box side, so 50% keeps a pill/circle shape)."""
    raw = node.style.get("border-radius", "")
    if not raw:
        return 0.0
    tok = raw.split("/")[0].split()
    if not tok:
        return 0.0
    t = tok[0].casefold()
    if t.endswith("%"):
        try:
            r = min(w, h) * float(t[:-1]) / 100.0
        except ValueError:
            return 0.0
    else:
        r = parse_size(t) or 0.0
    return max(0.0, min(r, min(w, h) / 2.0))


def safe_color(value, default="black"):
    if not value:
        return default
    value = value.strip().casefold()
    if value in NON_COLOR_KEYWORDS:
        return default
    if re.fullmatch(r"#[0-9a-f]{3}([0-9a-f]{3})?", value):
        return value
    m = re.fullmatch(
        r"rgba?\(\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)\s*(?:,\s*([\d.]+)\s*)?\)",
        value)
    if m:
        # fully transparent -> nothing to paint (e.g. naver's search box
        # background is rgba(0,0,0,0); painting it opaque = a black box)
        if m.group(4) is not None:
            try:
                if float(m.group(4)) <= 0:
                    return default
            except ValueError:
                pass
        r, g, b = (min(int(m.group(i)), 255) for i in (1, 2, 3))
        return f"#{r:02x}{g:02x}{b:02x}"
    if value in NAMED:
        # named color both backends know (an arbitrary word here would
        # crash tk with TclError and paint black on the native path)
        return value
    return default


def box_shadow(value):
    """Parse the first box-shadow layer to (dx, dy, color), or None.
    Blur/spread are ignored (we paint a flat offset rect); `inset` and
    `none` yield None."""
    v = (value or "").split(",")[0].strip()
    if not v or v == "none" or "inset" in v:
        return None
    color = ""
    nums = []
    for tok in v.split():
        c = safe_color(tok, default="")
        if c and not color:
            color = c
        elif tok.endswith("px") or _num_re.fullmatch(tok):
            try:
                nums.append(float(tok.rstrip("px")))
            except ValueError:
                pass
    if len(nums) < 2:
        return None
    return nums[0], nums[1], color or "#000000"


_num_re = re.compile(r"-?\d+(?:\.\d+)?")


def gradient_color(value, default=""):
    """A gradient can't be painted, but its first color stop is a good
    solid-fill approximation (buttons/headers read almost the same)."""
    if not value or "gradient(" not in value.casefold():
        return default
    # first #hex, rgb()/rgba(), or named color inside the parens
    inner = value[value.find("(") + 1:]
    for m in re.finditer(
            r"#[0-9a-fA-F]{3,8}|rgba?\([^)]*\)|[a-zA-Z]+", inner):
        c = safe_color(m.group(0), default="")
        if c:
            return c
    return default


_REPEAT_RE = re.compile(r"repeat\(\s*(\d+)\s*,([^)]*)\)")


def layout_mode(node):
    if isinstance(node, Text):
        return "inline"
    display = node.style.get("display", "")
    if display in ("flex", "inline-flex") and node.children:
        return "flex"
    if display == "block" and node.children:
        return "block"
    if any(isinstance(child, Element) and (
            child.tag in BLOCK_ELEMENTS
            or child.style.get("display", "") in ("block", "flex"))
           for child in node.children):
        return "block"
    if node.tag in ("svg", "::before", "::after"):
        return "inline"  # replaced/synthesized: inline by nature
    if node.children or node.tag in ("br", "hr", "input", "img"):
        return "inline"
    return "block"


def is_visible(node):
    return node.style.get("display", "inline") != "none"


def is_out_of_flow(node):
    return (isinstance(node, Element)
            and node.style.get("position") in ("absolute", "fixed"))


def translate(layout_obj, dx, dy):
    layout_obj.x += dx
    layout_obj.y += dy
    for child in layout_obj.children:
        translate(child, dx, dy)


def apply_relative_offsets(children):
    """Shift position:relative boxes only after every sibling has been
    placed, so the offset is visual-only and never moves in-flow
    layout (siblings keep their pre-translate positions)."""
    for child in children:
        dx = getattr(child, "rel_dx", 0)
        dy = getattr(child, "rel_dy", 0)
        if dx or dy:
            translate(child, dx, dy)


class DocumentLayout:
    def __init__(self, node):
        self.node = node
        self.parent = None
        self.children = []
        self.x = 0
        self.y = 0
        self.width = 0
        self.height = 0
        self.abs_queue = []
        self.viewport_width = None
        self.viewport_height = None
        self.definite_height = None

    def layout(self, width, height=None):
        self.width = width - 2 * HSTEP
        self.x = HSTEP
        self.y = VSTEP
        self.viewport_width = width
        self.viewport_height = height
        self.definite_height = (max(height - 2 * VSTEP, 0)
                                if height is not None else None)
        self.children = []
        self.abs_queue = []
        child = BlockLayout(self.node, self, None)
        self.children.append(child)
        child.layout()
        self.height = child.outer_height()
        apply_relative_offsets(self.children)
        self._layout_positioned()

    def _layout_positioned(self):
        """Lay out position:absolute/fixed boxes queued during layout.
        Containing block ~ the document (approximation).

        Laying out an out-of-flow box can queue *more* out-of-flow
        descendants onto abs_queue, so we drain it by index and process
        each node at most once (nested absolutes on real pages like
        naver would otherwise loop forever). A hard cap bounds any
        pathological page."""
        processed = set()
        i = 0
        while i < len(self.abs_queue) and len(processed) < 5000:
            node = self.abs_queue[i]
            i += 1
            if id(node) in processed:
                continue
            processed.add(id(node))
            st = node.style
            em = parse_px(st.get("font-size", "16px"), 16.0)
            left = parse_size(st.get("left"), self.width, em)
            right = parse_size(st.get("right"), self.width, em)
            top = parse_size(st.get("top"), self.height, em)
            bottom = parse_size(st.get("bottom"), self.height, em)
            if left is None and right is None and top is None \
                    and bottom is None:
                # no offsets: skip (avoids overlaying the static flow)
                continue
            box = BlockLayout(node, self, None)
            spec_w = parse_size(st.get("width"), self.width, em)
            if spec_w is not None:
                box.forced_width = spec_w
            elif left is not None and right is not None:
                box.forced_width = max(self.width - left - right, 40)
            else:
                box.forced_width = max(
                    self.width - (left or right or 0), 40)
            self.children.append(box)
            box.layout()

            if left is not None:
                target_x = self.x + left
            elif right is not None:
                target_x = self.x + self.width - right - box.outer_width()
            else:
                target_x = self.x
            if top is not None:
                target_y = self.y + top
            elif bottom is not None:
                target_y = self.y + self.height - bottom \
                    - box.outer_height()
            else:
                target_y = self.y
            translate(box,
                      target_x - (box.x - box.pl - box.bw - box.ml),
                      target_y - (box.y - box.pt - box.bw
                                  - box.margin_top))

    def paint(self):
        return []


class BlockLayout:
    def __init__(self, node, parent, previous):
        self.node = node
        self.parent = parent
        self.previous = previous
        self.children = []
        self.x = 0
        self.y = 0
        self.width = 0
        self.height = 0
        self.margin_top = 0
        self.margin_bottom = 0
        self.ml = 0
        self.mr = 0
        self.pt = self.pr = self.pb = self.pl = 0
        self.bw = 0
        self.forced_width = None   # border-box width imposed by flex/abs
        self.flex_origin = None    # (x, y) margin-edge origin from flex
        self.flex_auto_margin = 0  # px one auto margin gets on a flex line
        self.rel_dx = 0            # position:relative visual offset,
        self.rel_dy = 0            # applied by the parent after layout
        self.definite_height = None  # content height when specified

    def outer_height(self):
        return (self.margin_top + self.bw + self.pt + self.height
                + self.pb + self.bw + self.margin_bottom)

    def outer_width(self):
        return (self.ml + self.bw + self.pl + self.width
                + self.pr + self.bw + self.mr)

    def _document(self):
        node = self.parent
        while not isinstance(node, DocumentLayout):
            node = node.parent
        return node

    def layout(self):
        node = self.node
        st = node.style
        em = parse_px(st.get("font-size", "16px"), 16.0)
        avail = self.parent.width

        def size(prop, base=avail):
            return parse_size(st.get(prop), base, em)

        self.pt = size("padding-top") or 0
        self.pr = size("padding-right") or 0
        self.pb = size("padding-bottom") or 0
        self.pl = size("padding-left") or 0
        self.bw = size("border-width", 0) or 0
        self.margin_top = size("margin-top") or 0
        self.margin_bottom = size("margin-bottom") or 0
        ml_raw = st.get("margin-left", "0").strip()
        mr_raw = st.get("margin-right", "0").strip()
        self.ml = size("margin-left") or 0
        self.mr = size("margin-right") or 0

        edge = self.pl + self.pr + 2 * self.bw
        spec = size("width")
        maxw = size("max-width")
        minw = size("min-width")
        if self.forced_width is not None:
            box_w = self.forced_width
        elif spec is not None:
            box_w = spec  # treated as border-box (web reality)
        else:
            box_w = avail - self.ml - self.mr
        if maxw is not None:
            box_w = min(box_w, maxw)
        if minw is not None:
            box_w = max(box_w, minw)
        box_w = max(box_w, edge)

        # auto horizontal margins: flex items absorb their line's free
        # space (resolved by _layout_flex); block boxes center in the
        # containing block
        if self.flex_origin is not None:
            if ml_raw == "auto":
                self.ml = self.flex_auto_margin
            if mr_raw == "auto":
                self.mr = self.flex_auto_margin
        elif spec is not None or maxw is not None \
                or self.forced_width is not None:
            space = avail - box_w
            if ml_raw == "auto" and mr_raw == "auto":
                self.ml = self.mr = max(space / 2, 0)
            elif ml_raw == "auto":
                self.ml = max(space - self.mr, 0)
            elif mr_raw == "auto":
                self.mr = max(space - self.ml, 0)

        if self.flex_origin is not None:
            base_x, base_y = self.flex_origin
            self.x = base_x + self.ml + self.bw + self.pl
            self.y = base_y + self.margin_top + self.bw + self.pt
        else:
            self.x = self.parent.x + self.ml + self.bw + self.pl
            if self.previous:
                p = self.previous
                # adjoining sibling margins collapse: both positive ->
                # max, both negative -> min, mixed -> sum (CSS 2.1)
                m1, m2 = p.margin_bottom, self.margin_top
                if m1 >= 0 and m2 >= 0:
                    coll = max(m1, m2)
                elif m1 < 0 and m2 < 0:
                    coll = min(m1, m2)
                else:
                    coll = m1 + m2
                self.y = (p.y + p.height + p.pb + p.bw
                          + coll + self.bw + self.pt)
            else:
                self.y = (self.parent.y + self.margin_top
                          + self.bw + self.pt)
            # clear: drop below the matching floats (set by the parent)
            floor = getattr(self, "clear_y_floor", None)
            if floor is not None:
                self.y = max(
                    self.y, floor + self.margin_top + self.bw + self.pt)
            # floats in the containing block carve the line area:
            # auto-width in-flow blocks shift and narrow around any
            # float overlapping their top edge (line-box granularity
            # is out of scope — the whole block sidesteps)
            pf = getattr(self.parent, "_floats", None)
            if pf and spec is None and self.forced_width is None \
                    and getattr(self, "float_side", None) is None:
                left_edge = self.parent.x
                right_edge = self.parent.x + self.parent.width
                for (s, fx, fy, fw2, fh) in pf:
                    if not (fy <= self.y < fy + fh):
                        continue
                    if s == "left":
                        left_edge = max(left_edge, fx + fw2)
                    else:
                        right_edge = min(right_edge, fx)
                shift = left_edge - self.parent.x
                narrow = (self.parent.x + self.parent.width) \
                    - right_edge
                if shift or narrow:
                    self.x += shift
                    box_w = max(box_w - shift - narrow, edge)
        self.width = max(box_w - edge, 0)

        # resolve a specified height before children lay out so they
        # can resolve percentage heights against it
        spec_h = self._specified_height(st, em)
        if spec_h is not None:
            self.definite_height = max(
                spec_h - self.pt - self.pb - 2 * self.bw, 0)

        mode = layout_mode(node)
        if isinstance(node, Element) and self._table_rows(node):
            self._layout_table(node, em)
        elif isinstance(node, Element) and node.children \
                and st.get("display", "").strip() == "grid":
            self._layout_grid(node, em)
        elif mode == "flex":
            self._layout_flex(node, em)
        elif mode == "block":
            # incremental placement: floats registered by earlier
            # children must be visible while later siblings lay out
            previous = None
            self._floats = []  # (side, x, y, outer_w, outer_h)
            for child in node.children:
                if not is_visible(child):
                    continue
                if is_out_of_flow(child):
                    self._document().abs_queue.append(child)
                    continue
                fside = "none"
                fw = None
                if isinstance(child, Element):
                    fside = child.style.get(
                        "float", "none").strip().casefold()
                    if fside in ("left", "right"):
                        fw = parse_size(
                            child.style.get("width"), self.width, em)
                if fside in ("left", "right") and fw is not None:
                    # float: out of normal flow, anchored where the
                    # flow currently ends (auto-width floats fall back
                    # to normal flow — v1 gate)
                    cur_y = self.y if previous is None else (
                        previous.y + previous.height + previous.pb
                        + previous.bw + previous.margin_bottom)
                    box = BlockLayout(child, self, None)
                    box.forced_width = fw
                    box.float_side = fside
                    box.flex_origin = (
                        self._float_x(fside, cur_y, fw), cur_y)
                    self.children.append(box)
                    box.layout()
                    self._floats.append((
                        fside,
                        box.x - box.ml - box.bw - box.pl,
                        cur_y,
                        box.outer_width(),
                        box.outer_height(),
                    ))
                    continue  # previous unchanged: out of flow
                nxt = BlockLayout(child, self, previous)
                if isinstance(child, Element) and self._floats:
                    cl = child.style.get(
                        "clear", "none").strip().casefold()
                    if cl in ("left", "right", "both"):
                        ys = [fy + fh
                              for (s, _, fy, _, fh) in self._floats
                              if cl == "both" or s == cl]
                        if ys:
                            nxt.clear_y_floor = max(ys)
                self.children.append(nxt)
                nxt.layout()
                previous = nxt
            # content height: bottom edge of the flow, extended to
            # cover float bottoms (clearfix-style containment)
            flow_bottom = self.y if previous is None else (
                previous.y + previous.height + previous.pb
                + previous.bw + previous.margin_bottom)
            float_bottom = max(
                (fy + fh for (_, _, fy, _, fh) in self._floats),
                default=self.y)
            self.height = max(flow_bottom, float_bottom) - self.y
            apply_relative_offsets(self.children)
        else:
            self.new_line()
            self.recurse(node)
            for line in self.children:
                line.layout()
            self.height = sum(line.height for line in self.children)

        if self.definite_height is not None:
            self.height = self.definite_height

        # position: relative offsets the box visually; stored here and
        # applied by the parent after all siblings are placed
        if st.get("position") == "relative":
            dx = size("left")
            if dx is None:
                r = size("right")
                dx = -r if r is not None else 0
            dy = parse_size(st.get("top"), 0, em)
            if dy is None:
                b = parse_size(st.get("bottom"), 0, em)
                dy = -b if b is not None else 0
            self.rel_dx, self.rel_dy = dx, dy

    def _specified_height(self, st, em):
        """CSS height as a border-box px value, or None for auto.
        Percentages resolve against the containing block's definite
        height and vh/vw against the viewport; when the base is
        unknown the height stays auto instead of collapsing to zero."""
        raw = (st.get("height") or "").strip().casefold()
        if not raw:
            return None
        if raw.endswith("vh") or raw.endswith("vw"):
            doc = self._document()
            base = (doc.viewport_height if raw.endswith("vh")
                    else doc.viewport_width)
            return parse_size(raw, base, em) if base is not None else None
        if raw.endswith("%"):
            base = self.parent.definite_height
            return parse_size(raw, base, em) if base is not None else None
        return parse_size(raw, 0, em)

    def _float_x(self, side, y, w):
        """Margin-edge x for a new float: after the floats already
        occupying this y, from the matching side."""
        left_edge = self.x
        right_edge = self.x + self.width
        for (s, fx, fy, fw, fh) in self._floats:
            if not (fy <= y < fy + fh):
                continue
            if s == "left":
                left_edge = max(left_edge, fx + fw)
            else:
                right_edge = min(right_edge, fx)
        if side == "left":
            return left_edge
        return max(right_edge - w, left_edge)

    def _layout_flex(self, node, em):
        """Simplified flexbox: row direction, optional wrap, grow."""
        kid_nodes = []
        for child in node.children:
            if not is_visible(child):
                continue
            if is_out_of_flow(child):
                self._document().abs_queue.append(child)
                continue
            if isinstance(child, Text) and not child.text.strip():
                continue
            kid_nodes.append(child)

        if node.style.get("flex-direction", "row").startswith("column"):
            # column flex behaves like block stacking
            previous = None
            for child in kid_nodes:
                nxt = BlockLayout(child, self, previous)
                self.children.append(nxt)
                previous = nxt
            for child in self.children:
                child.layout()
            self.height = sum(
                child.outer_height() for child in self.children)
            apply_relative_offsets(self.children)
            return

        # main size: flex-basis wins over width; None = content-sized
        specs = []
        grows = []
        shrinks = []
        for child in kid_nodes:
            basis = child.style.get("flex-basis", "")
            spec = parse_size(basis, self.width, em) \
                if basis else None
            if spec is None:
                spec = parse_size(
                    child.style.get("width"), self.width, em)
            specs.append(spec)

            def _num(v, default):
                try:
                    return max(float(v), 0.0)
                except (TypeError, ValueError):
                    return default
            grows.append(_num(child.style.get("flex-grow"), 0.0))
            shrinks.append(_num(child.style.get("flex-shrink"), 1.0))

        fixed_total = sum(s for s in specs if s is not None)
        n_flex = sum(1 for s in specs if s is None)
        # note: match exact keywords — "wrap" is a substring of
        # "nowrap", which silently forced every container to wrap
        wrap_v = node.style.get("flex-wrap", "nowrap").strip().casefold()
        wrap = wrap_v in ("wrap", "wrap-reverse")
        total_grow = sum(grows)

        if total_grow > 0 and not wrap:
            # spec-style grow: positive free space distributed by
            # factor. Content-sized items keep the legacy equal share
            # so mixed rows still work.
            legacy = 0.0
            if n_flex:
                legacy = max((self.width - fixed_total) / n_flex, 40.0)
            widths = [s if s is not None else legacy for s in specs]
            free = self.width - sum(widths)
            if free > 0:
                for i, g in enumerate(grows):
                    widths[i] += free * g / total_grow
            specs = widths
            fixed_total = sum(specs)
            n_flex = 0
        grow = 0.0
        if n_flex:
            grow = max((self.width - fixed_total) / n_flex, 40.0)

        if not wrap:
            # flex-shrink: a single overflowing line gives space back
            # in proportion to shrink-factor x main size
            sized = [s if s is not None else grow for s in specs]
            overflow = sum(sized) - self.width
            weights = [shrinks[i] * sized[i] for i in range(len(sized))]
            wsum = sum(weights)
            if overflow > 0 and wsum > 0:
                specs = [
                    max(sized[i] - overflow * weights[i] / wsum, 0.0)
                    for i in range(len(sized))
                ]
                fixed_total = sum(specs)
                n_flex = 0
                grow = 0.0

        # auto margins on flex items split the line's leftover space;
        # when grow items consume it (or items overflow into wrapping)
        # the free space is <= 0 and auto margins resolve to zero
        n_auto = sum(
            (child.style.get("margin-left", "").strip().casefold()
             == "auto")
            + (child.style.get("margin-right", "").strip().casefold()
               == "auto")
            for child in kid_nodes)
        auto_px = 0.0
        if n_auto:
            free = self.width - fixed_total - grow * n_flex
            auto_px = max(free / n_auto, 0.0)

        cx, row_y, row_h = self.x, self.y, 0.0
        rows = []      # [(boxes, row_height)]
        row_boxes = []
        for child, spec in zip(kid_nodes, specs):
            w = spec if spec is not None else grow
            if wrap and cx + w > self.x + self.width and cx > self.x:
                rows.append((row_boxes, row_h))
                row_boxes = []
                row_y += row_h
                cx, row_h = self.x, 0.0
            box = BlockLayout(child, self, None)
            box.forced_width = w
            box.flex_auto_margin = auto_px
            box.flex_origin = (cx, row_y)
            self.children.append(box)
            box.layout()
            row_boxes.append(box)
            cx += box.outer_width()
            row_h = max(row_h, box.outer_height())
        if row_boxes:
            rows.append((row_boxes, row_h))
        self.height = (row_y + row_h) - self.y

        # justify-content: distribute each row's leftover main-axis
        # space (unless auto margins already absorbed it);
        # align-items/align-self: cross-axis position within the row.
        # Both are post-placement subtree shifts — no relayout.
        justify = node.style.get("justify-content",
                                 "flex-start").strip().casefold()
        align = node.style.get("align-items",
                               "stretch").strip().casefold()
        for boxes, height in rows:
            if not boxes:
                continue
            if justify not in ("", "flex-start", "start", "normal",
                               "left") and not n_auto:
                used = sum(b.outer_width() for b in boxes)
                free = max(self.width - used, 0.0)
                lead, gap = 0.0, 0.0
                if justify in ("center",):
                    lead = free / 2
                elif justify in ("flex-end", "end", "right"):
                    lead = free
                elif justify == "space-between":
                    gap = free / (len(boxes) - 1) if len(boxes) > 1 \
                        else 0.0
                elif justify in ("space-around", "space-evenly"):
                    n = len(boxes)
                    if justify == "space-around":
                        gap = free / n
                        lead = gap / 2
                    else:
                        gap = free / (n + 1)
                        lead = gap
                if lead or gap:
                    for i, b in enumerate(boxes):
                        translate(b, lead + gap * i, 0)
            for b in boxes:
                mode = b.node.style.get("align-self",
                                        "").strip().casefold() or align
                if mode in ("center",):
                    dy = (height - b.outer_height()) / 2
                elif mode in ("flex-end", "end"):
                    dy = height - b.outer_height()
                else:  # stretch/flex-start/baseline: top (no
                    #     cross-size stretching yet)
                    dy = 0.0
                if dy > 0:
                    translate(b, 0, dy)
        apply_relative_offsets(self.children)

    # ----- table layout (v1) -----

    def _table_rows(self, node):
        """The <tr> rows of a table box, walking through row groups.
        A box is a table when it's <table> or display:table AND has at
        least one row — otherwise it stays in normal block flow."""
        st = node.style
        if node.tag != "table" and st.get("display", "").strip() != "table":
            return []
        rows = []

        def collect(n):
            for c in n.children:
                if not isinstance(c, Element) or not is_visible(c):
                    continue
                if c.tag == "tr" \
                        or c.style.get("display", "") == "table-row":
                    rows.append(c)
                elif c.tag in ("thead", "tbody", "tfoot"):
                    collect(c)

        collect(node)
        return rows

    def _layout_table(self, node, em):
        """Fixed-grid table v1: every column gets an equal share of the
        content width (colspan spans that many shares); the row advances
        by its tallest cell. No auto column sizing, border-spacing, or
        rowspan yet."""
        rows = self._table_rows(node)

        def cells_of(tr):
            return [c for c in tr.children if isinstance(c, Element)
                    and is_visible(c)
                    and (c.tag in ("td", "th") or c.style.get(
                        "display", "") == "table-cell")]

        def span_of(cell):
            try:
                return max(int(cell.attributes.get("colspan", 1)), 1)
            except (ValueError, TypeError):
                return 1

        ncols = max((sum(span_of(c) for c in cells_of(tr))
                     for tr in rows), default=0)
        if not ncols:
            self.height = 0
            return
        col_w = self.width / ncols
        y = self.y
        for tr in rows:
            x = self.x
            row_boxes = []
            for cell in cells_of(tr):
                w = col_w * min(span_of(cell), ncols)
                box = BlockLayout(cell, self, None)
                box.forced_width = w
                box.flex_origin = (x, y)
                self.children.append(box)
                box.layout()
                row_boxes.append(box)
                x += w
            y += max((b.outer_height() for b in row_boxes), default=0)
        self.height = y - self.y
        apply_relative_offsets(self.children)

    # ----- grid layout (v1) -----

    def _grid_tracks(self, spec, em, gap):
        """grid-template-columns -> list of track widths in px.
        Supports px/%/em, fr, auto (=1fr), and repeat(n, ...). Anything
        unparseable yields a single full-width track."""
        # expand repeat(n, X ...) — the parenthesized body has no
        # nesting we support, so a simple scan suffices
        expanded = []
        i = 0
        text = spec.strip()
        while i < len(text):
            m = _REPEAT_RE.match(text, i)
            if m:
                count = int(m.group(1))
                body = m.group(2).strip()
                expanded.extend(body.split() * max(count, 0))
                i = m.end()
            else:
                j = text.find(" ", i)
                if j == -1:
                    expanded.append(text[i:])
                    break
                if text[i:j]:
                    expanded.append(text[i:j])
                i = j + 1
        fixed = []
        total_fr = 0.0
        for tok in expanded:
            tok = tok.strip()
            if not tok:
                continue
            if tok.endswith("fr"):
                try:
                    total_fr += max(float(tok[:-2]), 0.0)
                    fixed.append(("fr", max(float(tok[:-2]), 0.0)))
                    continue
                except ValueError:
                    pass
            if tok == "auto":
                total_fr += 1.0
                fixed.append(("fr", 1.0))
                continue
            px = parse_size(tok, self.width, em)
            if px is None:
                return []
            fixed.append(("px", px))
        if not fixed:
            return []
        gaps = gap * (len(fixed) - 1)
        free = max(self.width - gaps
                   - sum(v for (k, v) in fixed if k == "px"), 0.0)
        return [v if k == "px" else
                (free * v / total_fr if total_fr else 0.0)
                for (k, v) in fixed]

    def _layout_grid(self, node, em):
        """display:grid v1: children fill the template's columns in
        row-major order; each row advances by its tallest item. No
        grid-area/span placement, dense packing, or implicit-track
        sizing beyond auto=1fr."""
        st = node.style
        gap_raw = st.get("gap", st.get("grid-gap", "")).split()
        row_gap = parse_size(
            st.get("row-gap", gap_raw[0] if gap_raw else ""),
            self.width, em) or 0
        col_gap = parse_size(
            st.get("column-gap",
                   gap_raw[1] if len(gap_raw) > 1
                   else (gap_raw[0] if gap_raw else "")),
            self.width, em) or 0
        tracks = self._grid_tracks(
            st.get("grid-template-columns", ""), em, col_gap)
        if not tracks:
            tracks = [self.width]
        items = [c for c in node.children
                 if isinstance(c, Element) and is_visible(c)
                 and not is_out_of_flow(c)]
        for c in node.children:
            if is_out_of_flow(c):
                self._document().abs_queue.append(c)
        y = self.y
        col = 0
        row_boxes = []
        x_starts = []
        run = self.x
        for w in tracks:
            x_starts.append(run)
            run += w + col_gap
        for item in items:
            if col >= len(tracks):
                y += max((b.outer_height() for b in row_boxes),
                         default=0) + row_gap
                row_boxes = []
                col = 0
            box = BlockLayout(item, self, None)
            box.forced_width = tracks[col]
            box.flex_origin = (x_starts[col], y)
            self.children.append(box)
            box.layout()
            row_boxes.append(box)
            col += 1
        if row_boxes:
            y += max(b.outer_height() for b in row_boxes)
        self.height = y - self.y
        apply_relative_offsets(self.children)

    # ----- inline layout -----

    def new_line(self):
        self.cursor_x = 0
        last = self.children[-1] if self.children else None
        self.children.append(LineLayout(self.node, self, last))

    def recurse(self, node):
        if isinstance(node, Text):
            if node.style.get("white-space") == "pre":
                self.preformatted(node)
            else:
                for word in node.text.split():
                    self.word(node, word)
        else:
            if not is_visible(node):
                return
            if is_out_of_flow(node):
                self._document().abs_queue.append(node)
                return
            if node.tag == "br":
                self.new_line()
            elif (isinstance(node, Element)
                  and node is not self.node
                  and node.style.get("display", "").strip()
                  == "inline-block"
                  and self.inline_block(node)):
                return
            elif node.tag == "img":
                self.image(node)
            elif node.tag == "svg":
                # replaced element: the subtree was rasterized to an
                # image handle — never lay out path/defs children
                self.image(node)
                return
            elif node.tag in ("::before", "::after"):
                # synthesized icon: a fixed-size inline box painted by
                # its background layer; text content flows normally
                w = parse_size(node.style.get("width", ""), self.width)
                h = parse_size(node.style.get("height", ""))
                if w and h and getattr(node, "_bg", None):
                    if self.cursor_x + w > self.width \
                            and self.cursor_x > 0:
                        self.new_line()
                    line = self.children[-1]
                    prev = line.children[-1] if line.children else None
                    line.children.append(
                        ImageLayout(node, w, h, line, prev))
                    self.cursor_x += w
                    return
            for child in node.children:
                self.recurse(child)

    def word(self, node, word):
        font = cached_font(node)
        w = measure(font, word)
        nowrap = node.style.get("white-space") in ("nowrap", "pre")
        if not nowrap and self.cursor_x + w > self.width \
                and self.cursor_x > 0:
            self.new_line()
        line = self.children[-1]
        prev = line.children[-1] if line.children else None
        text = TextLayout(node, word, line, prev)
        line.children.append(text)
        self.cursor_x += w + measure(font, " ")

    def inline_block(self, node):
        """display:inline-block with a specified width rides the line
        as an atomic box (auto-width shrink-to-fit is not measured yet
        — those fall back to plain inline flow, the pre-v1 behavior).
        Returns True when the box was placed."""
        em = parse_px(node.style.get("font-size", "16px"), 16.0)
        w = parse_size(node.style.get("width"), self.width, em)
        if w is None:
            return False
        inner = BlockLayout(node, self, None)
        inner.forced_width = w
        inner.flex_origin = (0, 0)  # laid out at origin, moved later
        inner.layout()
        if self.cursor_x + inner.outer_width() > self.width \
                and self.cursor_x > 0:
            self.new_line()
        line = self.children[-1]
        prev = line.children[-1] if line.children else None
        line.children.append(InlineBlockLayout(inner, line, prev))
        self.cursor_x += inner.outer_width()
        return True

    def image(self, node):
        img = getattr(node, "_img", None)  # (image_id, w, h) or None
        natural_w, natural_h = (img[1], img[2]) if img else (24, 24)
        # CSS width/height outrank the HTML attributes on replaced
        # elements (% width against the containing block; % height has
        # no definite base here and falls through to the attr/ratio)
        em = parse_px(node.style.get("font-size", "16px"), 16.0)
        w = parse_size(node.style.get("width"), self.width, em)
        h = parse_size(node.style.get("height"), 0.0, em)
        w = w if w is not None else _attr_px(node, "width")
        h = h if h is not None else _attr_px(node, "height")
        if w and not h:
            h = w * natural_h / natural_w if natural_w else w
        elif h and not w:
            w = h * natural_w / natural_h if natural_h else h
        elif not w and not h:
            w, h = float(natural_w), float(natural_h)
        # never overflow the containing block
        if w > self.width and w > 0:
            scale = self.width / w
            w, h = w * scale, h * scale
        if self.cursor_x + w > self.width and self.cursor_x > 0:
            self.new_line()
        line = self.children[-1]
        prev = line.children[-1] if line.children else None
        line.children.append(ImageLayout(node, w, h, line, prev))
        self.cursor_x += w

    def preformatted(self, node):
        font = cached_font(node)
        for i, raw_line in enumerate(node.text.split("\n")):
            if i > 0:
                self.new_line()
            if not raw_line:
                continue
            line = self.children[-1]
            prev = line.children[-1] if line.children else None
            text = TextLayout(node, raw_line, line, prev, keep_spaces=True)
            line.children.append(text)
            self.cursor_x += measure(font, raw_line)

    # ----- painting -----

    def paint(self):
        cmds = []
        if isinstance(self.node, Element):
            # opacity composites the whole subtree; the paint-relevant
            # case is "effectively invisible" (naver hides shell pieces
            # with opacity:0 until its app boots)
            if effective_opacity(self.node) < 0.05:
                return cmds
            # padding box corners
            x1 = self.x - self.pl
            y1 = self.y - self.pt
            x2 = self.x + self.width + self.pr
            y2 = self.y + self.height + self.pb

            bgcolor = self.node.style.get("background-color")
            if not bgcolor and "background" in self.node.style:
                for token in self.node.style["background"].split():
                    if safe_color(token, default=""):
                        bgcolor = token
                        break
            color = safe_color(bgcolor, default="") if bgcolor else ""
            if not color:
                # gradient background -> first stop as a solid fill
                for prop in ("background-image", "background", "background-color"):
                    color = gradient_color(
                        self.node.style.get(prop, ""), default="")
                    if color:
                        break
            radius = corner_radius(self.node, x2 - x1, y2 - y1)
            b = self.bw
            bcolor = safe_color(
                self.node.style.get("border-color", "#999999"))

            # box-shadow: a flat offset rect behind the box (blur/spread
            # approximated away)
            shadow = box_shadow(self.node.style.get("box-shadow", ""))
            if shadow:
                dx, dy, scolor = shadow
                cmds.append(DrawRect(
                    x1 + dx, y1 + dy, x2 + dx, y2 + dy, scolor,
                    radius=radius))

            if radius > 0 and b > 0 and color:
                # rounded box: border ring = outer rounded fill,
                # then the background inset by the border width
                cmds.append(DrawRect(x1 - b, y1 - b, x2 + b, y2 + b,
                                     bcolor, radius=radius + b))
                cmds.append(DrawRect(x1, y1, x2, y2, color,
                                     radius=radius))
            else:
                if color:
                    cmds.append(DrawRect(x1, y1, x2, y2, color,
                                         radius=radius))
                if b > 0:
                    cmds.append(DrawRect(x1 - b, y1 - b, x2 + b, y1,
                                         bcolor))
                    cmds.append(DrawRect(x1 - b, y2, x2 + b, y2 + b,
                                         bcolor))
                    cmds.append(DrawRect(x1 - b, y1, x1, y2, bcolor))
                    cmds.append(DrawRect(x2, y1, x2 + b, y2, bcolor))

            bg_img = paint_background_image(self.node, x1, y1, x2, y2)
            if bg_img:
                cmds.append(bg_img)

            if self.node.tag == "input":
                value = self.node.attributes.get("value", "")
                text = value or self.node.attributes.get("placeholder", "")
                font = cached_font(self.node)
                ty = self.y + max(
                    0.0, (self.height - font.metrics("linespace")) / 2)
                if text.strip():
                    tcolor = (safe_color(self.node.style.get("color"))
                              if value else "#9e9e9e")
                    cmds.append(DrawText(self.x, ty, text, font, tcolor))
                if getattr(self.node, "is_focused", False):
                    # caret after the typed value (not the placeholder)
                    cx = self.x + (measure(font, value) if value else 0)
                    ch = font.metrics("linespace")
                    cmds.append(DrawLine(
                        cx, ty, cx, ty + ch, "#333333", 1))

            if self.node.tag == "li":
                font = cached_font(self.node)
                r = 2
                cy = self.y + font.gg_linespace / 2
                cmds.append(DrawOval(
                    self.x - 12, cy - r, self.x - 12 + 2 * r, cy + r,
                    safe_color(self.node.style.get("color", "black"))))
            if self.node.tag == "hr":
                cmds.append(DrawLine(
                    self.x, self.y + 4, self.x + self.width, self.y + 4,
                    "#cccccc", 1))
                self.height = max(self.height, 9)

            # overflow:hidden clips descendants to the padding box
            if self._clips():
                cmds.append(DrawClipPush(x1, y1, x2, y2))
        return cmds

    def _clips(self):
        if not isinstance(self.node, Element):
            return False
        for axis in ("overflow", "overflow-x", "overflow-y"):
            # auto/scroll areas clip like hidden (v1: no inner
            # scrolling yet — content just can't bleed out)
            if self.node.style.get(axis) in ("hidden", "clip", "scroll",
                                             "auto"):
                return True
        return False

    def paint_after(self):
        return [DrawClipPop()] if self._clips() else []


class LineLayout:
    def __init__(self, node, parent, previous):
        self.node = node
        self.parent = parent
        self.previous = previous
        self.children = []
        self.x = 0
        self.y = 0
        self.width = 0
        self.height = 0
        self.margin_top = 0
        self.margin_bottom = 0

    def layout(self):
        self.width = self.parent.width
        self.x = self.parent.x
        if self.previous:
            self.y = self.previous.y + self.previous.height
        else:
            self.y = self.parent.y

        for word in self.children:
            word.layout()

        if not self.children:
            self.height = 0
            return

        def ascent(child):
            # images sit on the baseline: ascent = height, descent = 0
            return child.font.gg_ascent if child.font else child.height

        def descent(child):
            return child.font.gg_descent if child.font else 0

        max_ascent = max(ascent(w) for w in self.children)
        # line-height (inherited): a taller line box centers the text
        font_px = next((c.font.size for c in self.children if c.font),
                       parse_px(self.node.style.get("font-size", "16px"),
                                16.0))
        factor = line_height_factor(self.node, font_px)
        baseline = self.y + factor * max_ascent
        for word in self.children:
            word.y = baseline - ascent(word)
        max_descent = max(descent(w) for w in self.children)
        self.height = factor * (max_ascent + max_descent)

        # text-align
        align = self.node.style.get("text-align", "left")
        if align in ("center", "right") and self.children:
            last = self.children[-1]
            used = (last.x + last.width) - self.x
            free = self.width - used
            if free > 0:
                shift = free / 2 if align == "center" else free
                for word in self.children:
                    word.x += shift

        # inline-block items: x/y are settled now — move the inner
        # block tree into place
        for word in self.children:
            if hasattr(word, "finalize"):
                word.finalize()

    def paint(self):
        return []


class TextLayout:
    def __init__(self, node, word, parent, previous, keep_spaces=False):
        self.node = node
        self.word = word
        self.parent = parent
        self.previous = previous
        self.children = []
        self.keep_spaces = keep_spaces
        self.x = 0
        self.y = 0
        self.width = 0
        self.height = 0
        self.margin_top = 0
        self.margin_bottom = 0
        self.font = None

    def layout(self):
        self.font = cached_font(self.node)
        if self.previous:
            # previous may be an image (font=None): use our own font
            prev_font = self.previous.font or self.font
            space = 0 if self.keep_spaces else measure(prev_font, " ")
            self.x = self.previous.x + self.previous.width + space
        else:
            self.x = self.parent.x
        self.width = measure(self.font, self.word)
        self.height = self.font.gg_linespace

    def paint(self):
        if effective_opacity(self.node) < 0.05:
            return []
        color = safe_color(self.node.style.get("color", "black"))
        word = self.word
        # text-overflow:ellipsis — truncate a single-line overflow and
        # append "…". The property is not inherited, so read it off the
        # nearest element ancestor (the block container).
        anc = self.node.parent if isinstance(self.node, Text) else self.node
        ancestor_to = ""
        while anc is not None:
            if isinstance(anc, Element):
                ancestor_to = anc.style.get("text-overflow", "")
                break
            anc = anc.parent
        if ancestor_to == "ellipsis" \
                and self.node.style.get("white-space") == "nowrap":
            avail = (self.parent.x + self.parent.width) - self.x
            if avail > 0 and self.width > avail:
                ell = measure(self.font, "…")
                lo, hi = 0, len(word)
                while lo < hi:
                    mid = (lo + hi + 1) // 2
                    if measure(self.font, word[:mid]) + ell <= avail:
                        lo = mid
                    else:
                        hi = mid - 1
                word = word[:lo] + "…"
        cmds = [DrawText(self.x, self.y, word, self.font, color)]
        decoration = self.node.style.get("text-decoration", "none")
        if "underline" in decoration:
            y = self.y + self.font.gg_ascent + 2
            cmds.append(DrawLine(self.x, y, self.x + self.width, y, color))
        if "line-through" in decoration:
            y = self.y + self.height / 2
            cmds.append(DrawLine(self.x, y, self.x + self.width, y, color))
        return cmds


class InlineBlockLayout:
    """A line item wrapping a fully laid-out BlockLayout (at origin
    coords). The line assigns x via layout() and y via the baseline
    pass; finalize() then translates the inner tree into place. The
    inner box is our only child, so ancestor translate()/paint walks
    reach it naturally."""

    def __init__(self, inner, parent, previous):
        self.node = inner.node
        self.inner = inner
        self.parent = parent
        self.previous = previous
        self.children = [inner]
        self.font = None  # baseline sits at the bottom, image-style
        self.x = 0
        self.y = 0
        self.width = inner.outer_width()
        self.height = inner.outer_height()
        self.margin_top = 0
        self.margin_bottom = 0
        self._ox = 0.0  # inner tree's applied offset so far
        self._oy = 0.0

    def layout(self):
        if self.previous:
            self.x = self.previous.x + self.previous.width
        else:
            self.x = self.parent.x

    def finalize(self):
        dx, dy = self.x - self._ox, self.y - self._oy
        if dx or dy:
            translate(self.inner, dx, dy)
            self._ox, self._oy = self.x, self.y

    def paint(self):
        return []


class ImageLayout:
    def __init__(self, node, width, height, parent, previous):
        self.node = node
        self.parent = parent
        self.previous = previous
        self.children = []
        self.font = None  # marks image children for LineLayout
        self.x = 0
        self.y = 0
        self.width = width
        self.height = height
        self.margin_top = 0
        self.margin_bottom = 0

    def layout(self):
        if self.previous:
            self.x = self.previous.x + self.previous.width
        else:
            self.x = self.parent.x

    def paint(self):
        if effective_opacity(self.node) < 0.05:
            return []
        img = getattr(self.node, "_img", None)
        if img:
            return [DrawImage(self.x, self.y, self.width, self.height,
                              img[0])]
        # synthesized icon boxes paint their background layer
        if getattr(self.node, "_bg", None):
            cmd = paint_background_image(
                self.node, self.x, self.y,
                self.x + self.width, self.y + self.height)
            return [cmd] if cmd else []
        if self.node.tag in ("::before", "::after"):
            return []
        # broken/unloaded image placeholder
        return [
            DrawRect(self.x, self.y, self.x + self.width,
                     self.y + self.height, "#eeeeee"),
            DrawLine(self.x, self.y, self.x + self.width,
                     self.y + self.height, "#bbbbbb"),
        ]


def _attr_px(node, name):
    try:
        value = node.attributes.get(name, "").strip().replace("px", "")
        return float(value) if value else 0.0
    except ValueError:
        return 0.0


def _z_index(obj):
    """Stacking level of a layout subtree for z-index ordering. Only
    positioned boxes with an integer z-index leave 0; everything else
    keeps document order."""
    node = getattr(obj, "node", None)
    if not isinstance(node, Element):
        return 0
    if node.style.get("position", "static") == "static":
        return 0  # z-index has no effect on static boxes
    try:
        return int(node.style.get("z-index", "auto"))
    except (ValueError, TypeError):
        return 0


def parse_transform(value, w, h):
    """CSS transform -> (dx, dy, hidden). The translate components of
    translate/translateX/translateY/translate3d/matrix are honored
    (percentages resolve against the element's own border box, the
    spec's reference box for translate); a zero scale hides the
    subtree. Rotation, skew, and non-zero scales are ignored —
    sprite sheets position with translate, which is what ×646 of
    naver's usage is."""
    dx = dy = 0.0
    hidden = False

    def px(v, base):
        v = v.strip()
        try:
            if v.endswith("%"):
                return float(v[:-1]) / 100.0 * base
            if v.endswith("px"):
                return float(v[:-2])
            return float(v)
        except ValueError:
            return 0.0

    for m in re.finditer(r"([a-zA-Z0-9]+)\s*\(([^)]*)\)", value):
        fn = m.group(1).lower()
        args = [a for a in m.group(2).split(",") if a.strip()]
        if not args:
            continue
        if fn in ("translate", "translate3d"):
            dx += px(args[0], w)
            if len(args) > 1:
                dy += px(args[1], h)
        elif fn == "translatex":
            dx += px(args[0], w)
        elif fn == "translatey":
            dy += px(args[0], h)
        elif fn == "matrix" and len(args) == 6:
            dx += px(args[4], w)
            dy += px(args[5], h)
            if px(args[0], 1) == 0 and px(args[3], 1) == 0:
                hidden = True
        elif fn in ("scale", "scale3d", "scalex", "scaley"):
            if all(px(a, 1) == 0 for a in args[:2]):
                hidden = True
    return dx, dy, hidden


def paint_tree(layout_object, display_list):
    # transform applies to an element's principal box (blocks only —
    # line/text boxes share their block's node and must not re-apply)
    if isinstance(layout_object, BlockLayout):
        style = getattr(layout_object.node, "style", None)
        tf = style.get("transform") if style else None
        if tf and tf != "none":
            dx, dy, hidden = parse_transform(
                tf, layout_object.width, layout_object.height)
            if hidden:
                return display_list
            if dx or dy:
                sub = _paint_tree_inner(layout_object, [])
                translate_cmds(sub, dx, dy)
                display_list.extend(sub)
                return display_list
    return _paint_tree_inner(layout_object, display_list)


def _paint_tree_inner(layout_object, display_list):
    display_list.extend(layout_object.paint())
    # Paint each child subtree into its own group so positioned boxes
    # with a z-index can be reordered. Sorting per container (not
    # globally) approximates each positioned+z-index box establishing
    # its own stacking context; equal z keeps document order (stable).
    groups = []
    reorder = False
    for i, child in enumerate(layout_object.children):
        sub = []
        paint_tree(child, sub)
        z = _z_index(child)
        if z != 0:
            reorder = True
        groups.append((z, i, sub))
    if reorder:
        groups.sort(key=lambda g: (g[0], g[1]))
    for _z, _i, sub in groups:
        display_list.extend(sub)
    after = getattr(layout_object, "paint_after", None)
    if after is not None:
        display_list.extend(after())
    return display_list


def layout_tree_to_list(tree, out):
    out.append(tree)
    for child in tree.children:
        layout_tree_to_list(child, out)
    return out
