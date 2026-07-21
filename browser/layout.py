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
    """A linear gradient can't be painted, but its first color stop is a
    good solid-fill approximation (buttons/headers read almost the same).
    A conic gradient is an angular sweep — nearly always a decorative
    accent (corner ornaments, rings), so approximating it as a solid colour
    paints the whole box in one arc's hue; naver's ornaments filled the
    page purple over the news. Leave conic (and mostly-transparent) sweeps
    unpainted."""
    if not value or "gradient(" not in value.casefold():
        return default
    if "conic-gradient(" in value.casefold():
        return default
    # first #hex, rgb()/rgba(), or named color inside the parens
    inner = value[value.find("(") + 1:]
    for m in re.finditer(
            r"#[0-9a-fA-F]{3,8}|rgba?\([^)]*\)|[a-zA-Z]+", inner):
        c = safe_color(m.group(0), default="")
        if c:
            return c
    return default


def _child_is_block_level(child):
    """Whether a child forces its container into block flow. Computed
    display overrides the tag default, so `<li style="display:inline-block">`
    is inline-level (an atomic box on a line), not a block — otherwise a
    grid of inline-block cards would stack vertically."""
    if not isinstance(child, Element):
        return False
    d = child.style.get("display", "")
    if d in ("inline", "inline-block", "inline-flex", "inline-table"):
        return False
    if d in ("block", "flex", "grid", "table", "list-item"):
        return True
    return child.tag in BLOCK_ELEMENTS


class _FlowMarker:
    """Stand-in 'previous sibling' marking the bottom of an inline-block
    row, so the next in-flow block clears the row. Never painted — only its
    y/height feed the next box's vertical placement."""
    __slots__ = ("y", "height", "pb", "bw", "margin_bottom")

    def __init__(self, y, height):
        self.y = y
        self.height = height
        self.pb = 0
        self.bw = 0
        self.margin_bottom = 0


def _has_table_rows(node):
    """True when a display:table box has real table structure to lay out
    (a row or cell somewhere inside, directly or through a row group).
    A bare `::after{display:table}` clearfix or a `display:table`
    centering wrapper with only block children has none, and stays in
    normal flow instead of going through the table algorithm."""
    for child in node.children:
        if not isinstance(child, Element):
            continue
        d = child.style.get("display", "")
        if d in ("table-row", "table-cell"):
            return True
        if d in ("table-row-group", "table-header-group",
                 "table-footer-group") and _has_table_rows(child):
            return True
    return False


def layout_mode(node):
    if isinstance(node, Text):
        return "inline"
    display = node.style.get("display", "")
    if display in ("flex", "inline-flex") and node.children:
        return "flex"
    if display in ("table", "inline-table") and node.children \
            and _has_table_rows(node):
        return "table"
    # -webkit-line-clamp / display:-webkit-box establish an inline context
    # whose wrapped lines we clamp; route them to inline flow (only when
    # every child is inline-level, which card titles always are).
    if (display == "-webkit-box"
            or node.style.get("-webkit-line-clamp")
            or node.style.get("line-clamp")) \
            and node.children \
            and not any(_child_is_block_level(c) for c in node.children):
        return "inline"
    # A block box lays out in block flow when it contains a block-level
    # child; a block whose children are all inline-level (text, <a>, <b>,
    # <span>, images…) establishes an inline formatting context so they
    # share wrapped lines instead of each inline child being stacked on
    # its own line.
    if any(_child_is_block_level(child) for child in node.children):
        return "block"
    # inline-block / inline-flex / inline-table direct children are laid
    # out side by side (and wrapped into rows) by the block formatter's
    # inline-block cursor — the card/column-grid path. Keep those in block
    # flow rather than the line-based inline path.
    if any(isinstance(child, Element)
           and child.style.get("display", "") in (
               "inline-block", "inline-flex", "inline-table")
           and child.tag not in ("img", "svg", "br")
           for child in node.children):
        return "block"
    if node.tag in ("svg", "::before", "::after"):
        return "inline"  # replaced/synthesized: inline by nature
    if node.children or node.tag in ("br", "hr", "input", "img"):
        return "inline"
    return "block"


def is_visible(node):
    return node.style.get("display", "inline") != "none"


def _is_cjk(ch):
    """Whether a character is CJK/Hangul/Kana — scripts written without
    spaces, where a line may break between (almost) any two characters."""
    o = ord(ch)
    return (0x2E80 <= o <= 0x9FFF      # CJK radicals..unified ideographs
            or 0xAC00 <= o <= 0xD7A3   # Hangul syllables
            or 0x3040 <= o <= 0x30FF   # Hiragana + Katakana
            or 0x3000 <= o <= 0x303F   # CJK symbols/punctuation
            or 0xF900 <= o <= 0xFAFF   # CJK compatibility ideographs
            or 0xFF00 <= o <= 0xFFEF)  # halfwidth/fullwidth forms


def _has_cjk(word):
    return any(_is_cjk(c) for c in word)


def _nearest_positioned(layout_box):
    """The containing block of an absolutely-positioned box: the nearest
    ancestor layout box (inclusive of the box that queued it) whose node
    is positioned (position != static). None ⇒ the initial containing
    block (the document). Its geometry is read only after the in-flow
    pass, when every box is final."""
    node = layout_box
    while isinstance(node, BlockLayout):
        n = node.node
        if isinstance(n, Element) and n.style.get("position", "static") in (
                "relative", "absolute", "fixed", "sticky"):
            return node
        node = node.parent
    return None


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


class _MeasureHolder:
    """A throwaway layout parent used to size a subtree under a very wide
    constraint (max-content measurement for flex items). Chains to the
    real DocumentLayout only so BlockLayout._document() resolves."""
    __slots__ = ("parent", "x", "y", "width", "definite_height")

    def __init__(self, doc, width):
        self.parent = doc
        self.x = 0.0
        self.y = 0.0
        self.width = width
        self.definite_height = None


_MAXCONTENT = 100000.0


def _measure_content_width(node, doc):
    """Max-content (border-box) main size of `node`: lay its subtree out
    under a very wide constraint so text does not wrap, then return the
    widest content extent plus the node's own horizontal padding+border.
    Absolute descendants are captured and discarded so the trial layout
    never leaks boxes into the real render. Memoized per layout pass: a
    node's max-content size is width-independent, so nested flex never
    re-measures the same subtree (keeps deep flex trees from going
    quadratic)."""
    cache = getattr(doc, "_content_width_cache", None)
    if cache is not None:
        hit = cache.get(id(node))
        if hit is not None:
            return hit
    holder = _MeasureHolder(doc, _MAXCONTENT)
    box = BlockLayout(node, holder, None)
    saved = doc.abs_queue
    saved_m = getattr(doc, "_measuring", False)
    doc.abs_queue = []
    doc._measuring = True
    try:
        box.layout()
    finally:
        doc.abs_queue = saved
        doc._measuring = saved_m
    origin = box.x
    m = 0.0
    stack = list(box.children)
    while stack:
        b = stack.pop()
        if isinstance(b, (TextLayout, ImageLayout)):
            m = max(m, (b.x + getattr(b, "width", 0)) - origin)
        stack.extend(getattr(b, "children", []))
    result = m + box.pl + box.pr + 2 * box.bw
    if cache is not None:
        cache[id(node)] = result
    return result


def _measure_min_width(node, doc):
    """Min-content (border-box) width of `node`: lay its subtree out under
    a near-zero constraint so every breakable run wraps, then return the
    widest line (the longest unbreakable word or replaced element). This
    is the floor a table column may shrink to before its content
    overflows. Memoized per pass, alongside the max-content cache."""
    cache = getattr(doc, "_min_width_cache", None)
    if cache is not None:
        hit = cache.get(id(node))
        if hit is not None:
            return hit
    holder = _MeasureHolder(doc, 1.0)
    box = BlockLayout(node, holder, None)
    saved = doc.abs_queue
    saved_m = getattr(doc, "_measuring", False)
    doc.abs_queue = []
    doc._measuring = True
    try:
        box.layout()
    finally:
        doc.abs_queue = saved
        doc._measuring = saved_m
    origin = box.x
    m = 0.0
    stack = list(box.children)
    while stack:
        b = stack.pop()
        if isinstance(b, (TextLayout, ImageLayout)):
            m = max(m, getattr(b, "width", 0.0))
        stack.extend(getattr(b, "children", []))
    result = m + box.pl + box.pr + 2 * box.bw
    if cache is not None:
        cache[id(node)] = result
    return result


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
        self._content_width_cache = {}
        self._min_width_cache = {}
        self._measuring = False
        child = BlockLayout(self.node, self, None)
        self.children.append(child)
        child.layout()
        self.height = child.outer_height()
        apply_relative_offsets(self.children)
        self._layout_positioned()

    def _layout_positioned(self):
        """Lay out position:absolute/fixed boxes queued during layout.

        Each queued entry carries its containing block (the nearest
        positioned ancestor, resolved in _nearest_positioned) and the
        static position it would occupy in normal flow. Offsets and
        percentage sizes resolve against the containing block's padding
        box; an offset that is auto falls back to the static position, so
        a box positioned only to leave the flow still appears where it
        would have been rather than jumping to the page origin.

        Laying out an out-of-flow box can queue *more* out-of-flow
        descendants, so we drain the queue by index and process each node
        at most once (nested absolutes on real pages would otherwise loop
        forever). A hard cap bounds any pathological page."""
        # An absolute element's `height:100%` (or any %) resolves against
        # its containing block. When that block is the document — whose
        # height in a headless full-page render is the whole (tall) page —
        # a decorative `position:absolute; height:100%` overlay would
        # balloon to full height and paint over the content. Hide the
        # document's definite height during out-of-flow layout so those
        # percentages fall back to auto (content height).
        saved_definite_height = self.definite_height
        self.definite_height = None
        processed = set()
        i = 0
        while i < len(self.abs_queue) and len(processed) < 5000:
            entry = self.abs_queue[i]
            i += 1
            node, cb, static_x, static_y = entry
            if id(node) in processed:
                continue
            processed.add(id(node))

            # containing-block padding box (document/ICB when unpositioned)
            if isinstance(cb, BlockLayout):
                cb_x = cb.x - cb.pl
                cb_y = cb.y - cb.pt
                cb_w = cb.width + cb.pl + cb.pr
                cb_h = cb.height + cb.pt + cb.pb
            else:
                cb_x, cb_y, cb_w, cb_h = self.x, self.y, self.width, \
                    self.height

            st = node.style
            em = parse_px(st.get("font-size", "16px"), 16.0)
            left = parse_size(st.get("left"), cb_w, em)
            right = parse_size(st.get("right"), cb_w, em)
            top = parse_size(st.get("top"), cb_h, em)
            bottom = parse_size(st.get("bottom"), cb_h, em)
            if left is None and right is None and top is None \
                    and bottom is None:
                # no offsets: an absolute box used only to leave the flow.
                # Rendering it at its static position tends to overlay
                # hidden/duplicated overlay panels, so skip it (a box with
                # any explicit offset is positioned and does get placed).
                continue

            box = BlockLayout(node, self, None)
            spec_w = parse_size(st.get("width"), cb_w, em)
            if spec_w is not None:
                box.forced_width = spec_w
            elif left is not None and right is not None:
                box.forced_width = max(cb_w - left - right, 0.0)
            else:
                # auto width: shrink-to-fit, capped to the containing block
                try:
                    mc = _measure_content_width(node, self)
                except Exception:
                    mc = cb_w
                box.forced_width = max(min(mc, cb_w), 0.0)
            self.children.append(box)
            box.layout()

            if left is not None:
                target_x = cb_x + left
            elif right is not None:
                target_x = cb_x + cb_w - right - box.outer_width()
            else:
                target_x = static_x
            if top is not None:
                target_y = cb_y + top
            elif bottom is not None:
                target_y = cb_y + cb_h - bottom - box.outer_height()
            else:
                target_y = static_y
            translate(box,
                      target_x - (box.x - box.pl - box.bw - box.ml),
                      target_y - (box.y - box.pt - box.bw
                                  - box.margin_top))
        self.definite_height = saved_definite_height

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

    def _queue_abs(self, node, static_x, static_y):
        """Queue an out-of-flow child for the positioned-layout pass,
        remembering its containing block (nearest positioned ancestor)
        and its static position (where it would sit in normal flow, used
        when an offset is auto)."""
        cb = _nearest_positioned(self)
        self._document().abs_queue.append((node, cb, static_x, static_y))

    def layout(self):
        # idempotent: inline-block sizing lays a box out once to measure,
        # then again at its final position — start each pass from a clean
        # child list so content isn't duplicated.
        self.children = []
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
                # CSS 2.1 §8.3.1: adjacent vertical margins collapse. The
                # gap between two in-flow block siblings is a single
                # margin, not the sum — max of the positive parts plus the
                # min of the negative parts (so equal 16px margins give a
                # 16px gap, not 32px).
                mb, mt = p.margin_bottom, self.margin_top
                collapse = max(mb, mt, 0.0) + min(mb, mt, 0.0)
                self.y = (p.y + p.height + p.pb + p.bw + collapse
                          + self.bw + self.pt)
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
        if mode == "flex":
            self._layout_flex(node, em)
        elif mode == "table":
            self._layout_table(node, em)
        elif mode == "block":
            # incremental placement: floats registered by earlier
            # children must be visible while later siblings lay out
            previous = None
            self._floats = []  # (side, x, y, outer_w, outer_h)
            ib_x = None        # inline-block run cursor (None = no run open)
            ib_row_y = self.y
            ib_row_h = 0
            for child in node.children:
                if not is_visible(child):
                    continue
                if is_out_of_flow(child):
                    flow_y = self.y if previous is None else (
                        previous.y + previous.height + previous.pb
                        + previous.bw + previous.margin_bottom)
                    self._queue_abs(child, self.x, flow_y)
                    continue
                # inline-block children flow horizontally and wrap into
                # rows (card/column grids) rather than stacking. Each is a
                # shrink-to-fit block placed at the run cursor.
                cdisp = child.style.get("display", "") \
                    if isinstance(child, Element) else ""
                if cdisp in ("inline-block", "inline-flex", "inline-table") \
                        and child.tag not in ("br", "img", "svg"):
                    cem = parse_px(child.style.get("font-size"), em)
                    w = parse_size(child.style.get("width"), self.width, cem)
                    if w is None:
                        try:
                            w = min(
                                _measure_content_width(
                                    child, self._document()),
                                self.width)
                        except Exception:
                            w = self.width
                    if ib_x is None:
                        ib_row_y = self.y if previous is None else (
                            previous.y + previous.height + previous.pb
                            + previous.bw + previous.margin_bottom)
                        ib_x = 0
                        ib_row_h = 0
                    box = BlockLayout(child, self, None)
                    box.forced_width = max(w, 0.0)
                    box.flex_origin = (self.x + ib_x, ib_row_y)
                    box.layout()
                    if ib_x > 0 and ib_x + box.outer_width() > self.width:
                        # wrap: re-anchor on the next row
                        ib_row_y += ib_row_h
                        ib_x = 0
                        box.flex_origin = (self.x, ib_row_y)
                        box.layout()
                    self.children.append(box)
                    ib_x += box.outer_width()
                    ib_row_h = max(ib_row_h, box.outer_height())
                    previous = _FlowMarker(ib_row_y, ib_row_h)
                    continue
                ib_x = None  # a block child closes any open inline-block run
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
            # -webkit-line-clamp: N — keep the first N wrapped lines and
            # mark the overflow with an ellipsis (card titles clamp to 2
            # lines so a grid of cards stays uniform height).
            clamp = st.get("-webkit-line-clamp") or st.get("line-clamp")
            if clamp and clamp.strip().isdigit():
                n_lines = int(clamp)
                if n_lines >= 1 and len(self.children) > n_lines:
                    self.children = self.children[:n_lines]
                    last = self.children[-1]
                    words = [w for w in last.children
                             if isinstance(w, TextLayout)]
                    if words:
                        words[-1].word = words[-1].word.rstrip() + "…"
            for line in self.children:
                line.layout()
            self.height = sum(line.height for line in self.children)

        if self.definite_height is not None:
            self.height = self.definite_height

        # min-height / max-height clamp the used content height
        # (CSS 2.1 §10.7)
        maxh = self._content_height_limit(st.get("max-height"), em)
        if maxh is not None:
            self.height = min(self.height, maxh)
        minh = self._content_height_limit(st.get("min-height"), em)
        if minh is not None:
            self.height = max(self.height, minh)

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

    def _content_height_limit(self, raw, em):
        """A min-/max-height value as a *content* height px (the border
        box less this box's padding and border), or None when auto/none
        or its percentage base is unknown. Mirrors _specified_height's
        base selection so vh/vw and % resolve the same way."""
        raw = (raw or "").strip().casefold()
        if not raw or raw in ("none", "auto"):
            return None
        if raw.endswith("vh") or raw.endswith("vw"):
            doc = self._document()
            base = (doc.viewport_height if raw.endswith("vh")
                    else doc.viewport_width)
            box = parse_size(raw, base, em) if base is not None else None
        elif raw.endswith("%"):
            base = self.parent.definite_height
            box = parse_size(raw, base, em) if base is not None else None
        else:
            box = parse_size(raw, 0, em)
        if box is None:
            return None
        return max(box - self.pt - self.pb - 2 * self.bw, 0.0)

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
                self._queue_abs(child, self.x, self.y)
                continue
            if isinstance(child, Text) and not child.text.strip():
                continue
            kid_nodes.append(child)

        # gap / row-gap / column-gap reserve fixed space between flex
        # items before any free space is distributed (CSS Box Alignment
        # §8). The `gap` shorthand is "<row> <column>" (one value = both).
        gap_parts = node.style.get("gap", "").split()
        row_gap = parse_size(
            node.style.get("row-gap")
            or (gap_parts[0] if gap_parts else ""), self.width, em) or 0.0
        col_gap = parse_size(
            node.style.get("column-gap")
            or (gap_parts[1] if len(gap_parts) > 1
                else gap_parts[0] if gap_parts else ""),
            self.width, em) or 0.0

        if node.style.get("flex-direction", "row").startswith("column"):
            # column flex behaves like block stacking (with row-gap
            # inserted between items)
            previous = None
            for i, child in enumerate(kid_nodes):
                nxt = BlockLayout(child, self, previous)
                self.children.append(nxt)
                previous = nxt
            for child in self.children:
                child.layout()
            if row_gap and len(self.children) > 1:
                for i, child in enumerate(self.children):
                    if i:
                        translate(child, 0, row_gap * i)
            self.height = sum(
                child.outer_height() for child in self.children) \
                + row_gap * max(len(self.children) - 1, 0)
            apply_relative_offsets(self.children)
            return

        # main size: flex-basis (length) > width > max-content of the
        # item's own content. Every item gets a definite base size; then
        # flex-grow hands out positive free space and flex-shrink removes
        # overflow. Sizing content items to their content (not an equal
        # share of free space) is what lets a `flex:0 0 auto` label sit at
        # its natural width while a `flex:1 0 0` sibling grows to fill the
        # rest — without it the label swallowed the row and the grow item
        # collapsed to zero (naver's nav tabs stacked at one x).
        doc = self._document()
        specs = []
        grows = []
        shrinks = []
        is_auto = []  # content-sized (no length basis, no width)
        for child in kid_nodes:
            basis = child.style.get("flex-basis", "").strip().casefold()
            if basis and basis not in ("auto", "content", "max-content",
                                       "fit-content", "min-content"):
                base = parse_size(basis, self.width, em)
            else:
                base = None
            auto = base is None
            if base is None:
                base = parse_size(child.style.get("width"), self.width, em)
                auto = base is None
            if base is None:
                base = _measure_content_width(child, doc)
            is_auto.append(auto)
            specs.append(max(base or 0.0, 0.0))

            def _num(v, default):
                try:
                    return max(float(v), 0.0)
                except (TypeError, ValueError):
                    return default
            grows.append(_num(child.style.get("flex-grow"), 0.0))
            shrinks.append(_num(child.style.get("flex-shrink"), 1.0))

        # note: match exact keywords — "wrap" is a substring of
        # "nowrap", which silently forced every container to wrap
        wrap_v = node.style.get("flex-wrap", "nowrap").strip().casefold()
        wrap = wrap_v in ("wrap", "wrap-reverse")
        total_grow = sum(grows)

        # With no explicit flex-grow anywhere, content-sized (auto-basis)
        # items share leftover space equally — real browsers leave the gap,
        # but filling it keeps naive equal-column flex layouts working. When
        # any item *does* declare grow, honour it exactly so a `flex:1 0 0`
        # pane fills the row while a `flex:0 0 auto` label keeps its content
        # width (the naver nav-tab case).
        if total_grow > 0:
            eff_grows = grows
            eff_total = total_grow
        else:
            eff_grows = [1.0 if is_auto[i] else 0.0
                         for i in range(len(specs))]
            eff_total = sum(eff_grows)

        gap_total = col_gap * max(len(specs) - 1, 0)
        if not wrap:
            free = self.width - sum(specs) - gap_total
            if free > 0 and eff_total > 0:
                for i, g in enumerate(eff_grows):
                    specs[i] += free * g / eff_total
            elif free < 0:
                # remove overflow in proportion to shrink-factor x base
                weights = [shrinks[i] * specs[i]
                           for i in range(len(specs))]
                wsum = sum(weights)
                if wsum > 0:
                    specs = [
                        max(specs[i] + free * weights[i] / wsum, 0.0)
                        for i in range(len(specs))
                    ]
        grow = 0.0          # every item now carries a definite base size
        n_flex = 0
        fixed_total = sum(specs)

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
            free = self.width - fixed_total - grow * n_flex - gap_total
            auto_px = max(free / n_auto, 0.0)

        cx, row_y, row_h = self.x, self.y, 0.0
        rows = []      # [(boxes, row_height)]
        row_boxes = []
        for child, spec in zip(kid_nodes, specs):
            w = spec if spec is not None else grow
            gap_before = col_gap if row_boxes else 0.0
            if wrap and cx + gap_before + w > self.x + self.width \
                    and cx > self.x:
                rows.append((row_boxes, row_h))
                row_boxes = []
                row_y += row_h + row_gap
                cx, row_h = self.x, 0.0
                gap_before = 0.0
            cx += gap_before
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
                used = sum(b.outer_width() for b in boxes) \
                    + col_gap * max(len(boxes) - 1, 0)
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

    def _layout_table(self, node, em):
        """CSS table layout (auto algorithm). Rows are flattened through
        row groups; columns are sized from each cell's min/max-content
        width, then widened to fill an explicit table width or shrunk to
        fit an overflowing one. Cells are placed on a column/row grid
        (colspan and rowspan honoured), and each row's cells stretch to
        the row's height so backgrounds fill. Site-agnostic: HN's page,
        Wikipedia infoboxes, and any display:table layout all flow here
        instead of stacking their cells as blocks."""
        doc = self._document()
        style = node.style

        def disp(el):
            return el.style.get("display", "") if isinstance(el, Element) \
                else ""

        # --- gather captions and rows (flattening row groups) ---
        captions = []
        rows = []          # [(row_element_or_None, [cell_elements])]
        anon = []

        def flush_anon():
            if anon:
                rows.append((None, list(anon)))
                anon.clear()

        def cells_of(row_el):
            return [c for c in row_el.children
                    if isinstance(c, Element) and is_visible(c)
                    and disp(c) == "table-cell"]

        def collect(container):
            for c in container.children:
                if not (isinstance(c, Element) and is_visible(c)):
                    continue
                d = disp(c)
                if d in ("table-row-group", "table-header-group",
                         "table-footer-group"):
                    flush_anon()
                    collect(c)
                elif d == "table-row":
                    flush_anon()
                    rows.append((c, cells_of(c)))
                elif d == "table-cell":
                    anon.append(c)
                elif d == "table-caption":
                    captions.append(c)

        collect(node)
        flush_anon()
        if not rows:                      # nothing table-shaped: skip
            self.height = 0
            return

        # --- occupancy grid: assign each cell a (row, col), reserving the
        #     span of colspan/rowspan cells so later cells shift past them
        def ispan(el, name, cap):
            try:
                v = int(str(el.attributes.get(name, "1")).strip() or "1")
            except (ValueError, TypeError):
                v = 1
            return max(1, min(v, cap))

        nrows = len(rows)
        placed = []        # {el, r, c, cs, rs}
        occupied = set()
        ncols = 0
        for r, (_row_el, cell_els) in enumerate(rows):
            c = 0
            for el in cell_els:
                while (r, c) in occupied:
                    c += 1
                cs = ispan(el, "colspan", 1000)
                rs = ispan(el, "rowspan", nrows)
                placed.append({"el": el, "r": r, "c": c, "cs": cs,
                               "rs": rs})
                for dr in range(rs):
                    for dc in range(cs):
                        occupied.add((r + dr, c + dc))
                c += cs
                ncols = max(ncols, c)
        if ncols == 0:
            self.height = 0
            return

        # --- column widths from min/max content, colspans distributed ---
        pref = [0.0] * ncols
        minw = [0.0] * ncols
        for cell in placed:
            cell["_p"] = _measure_content_width(cell["el"], doc)
            cell["_m"] = _measure_min_width(cell["el"], doc)
            if cell["cs"] == 1:
                pref[cell["c"]] = max(pref[cell["c"]], cell["_p"])
                minw[cell["c"]] = max(minw[cell["c"]], cell["_m"])
        for cell in placed:
            if cell["cs"] == 1:
                continue
            cols = range(cell["c"], cell["c"] + cell["cs"])
            for arr, key in ((pref, "_p"), (minw, "_m")):
                cur = sum(arr[i] for i in cols)
                if cell[key] > cur:
                    add = (cell[key] - cur) / cell["cs"]
                    for i in cols:
                        arr[i] += add

        # --- border-spacing (separate model) or 0 when collapsed ---
        collapse = (style.get("border-collapse", "").strip().casefold()
                    == "collapse")
        s = 0.0 if collapse else self._table_spacing(node, em)
        spacing_x = s * (ncols + 1)

        avail = self.width
        explicit = (self.forced_width is not None
                    or bool(style.get("width"))
                    or bool(node.attributes.get("width")))
        content_avail = max(avail - spacing_x, 0.0)
        total_pref = sum(pref)
        total_min = sum(minw)
        if total_pref <= content_avail:
            widths = pref[:]
            extra = content_avail - total_pref
            if explicit and extra > 0:
                if total_pref > 0:
                    for i in range(ncols):
                        widths[i] += extra * pref[i] / total_pref
                else:
                    for i in range(ncols):
                        widths[i] += extra / ncols
        elif total_min <= content_avail:
            span = total_pref - total_min
            deficit = total_pref - content_avail
            widths = []
            for i in range(ncols):
                room = pref[i] - minw[i]
                cut = (deficit * room / span) if span > 0 \
                    else (deficit / ncols)
                widths.append(max(pref[i] - cut, minw[i]))
        else:
            widths = minw[:]     # overflow: honour min, table exceeds avail

        used_content = sum(widths)
        table_inner = used_content + spacing_x
        if not explicit:
            self.width = max(min(self.width, table_inner), 0.0)

        # column x offsets (from the table's content-left edge)
        col_x = [0.0] * ncols
        cx = s
        for i in range(ncols):
            col_x[i] = cx
            cx += widths[i] + s

        # --- captions above the rows ---
        self.children = []
        cap_h = 0.0
        for cap in captions:
            box = BlockLayout(cap, self, None)
            box.forced_width = self.width
            box.flex_origin = (self.x, self.y + cap_h)
            box.layout()
            self.children.append(box)
            cap_h += box.outer_height()

        # --- pass 1: lay out cells at natural size, gather row heights ---
        row_h = [0.0] * nrows
        for cell in placed:
            el, r, c, cs = cell["el"], cell["r"], cell["c"], cell["cs"]
            w = sum(widths[c:c + cs]) + s * (cs - 1)
            box = BlockLayout(el, self, None)
            box.forced_width = max(w, 0.0)
            box.flex_origin = (0.0, 0.0)
            box.layout()
            cell["box"] = box
            cell["outer_h"] = box.outer_height()
            if cell["rs"] == 1:
                row_h[r] = max(row_h[r], cell["outer_h"])
        # rowspan cells grow the last row they touch if they overflow
        for cell in placed:
            if cell["rs"] == 1:
                continue
            r, rs = cell["r"], cell["rs"]
            have = sum(row_h[r:r + rs]) + s * (rs - 1)
            if cell["outer_h"] > have:
                row_h[r + rs - 1] += cell["outer_h"] - have

        # row y offsets (from the table's content-top edge)
        row_y = [0.0] * nrows
        ry = cap_h + s
        for r in range(nrows):
            row_y[r] = ry
            ry += row_h[r] + s
        total_h = ry

        # --- pass 2: place row backgrounds and cells at final positions ---
        row_boxes = [None] * nrows
        for r, (row_el, _cells) in enumerate(rows):
            if row_el is None:
                continue
            rb = BlockLayout(row_el, self, None)
            rb.x = self.x
            rb.y = self.y + row_y[r]
            rb.width = self.width
            rb.height = row_h[r]
            row_boxes[r] = rb
            self.children.append(rb)
        for cell in placed:
            box, r, c = cell["box"], cell["r"], cell["c"]
            box.flex_origin = (self.x + col_x[c], self.y + row_y[r])
            box.layout()
            span_h = sum(row_h[r:r + cell["rs"]]) + s * (cell["rs"] - 1)
            fill = span_h - box.margin_top - box.margin_bottom \
                - 2 * box.bw - box.pt - box.pb
            if fill > box.height:
                box.height = fill
            parent = row_boxes[r]
            (parent.children if parent is not None
             else self.children).append(box)

        self.height = total_h
        apply_relative_offsets(self.children)

    def _table_spacing(self, node, em):
        """Horizontal/vertical gap between separated cells: the CSS
        border-spacing, else the legacy cellspacing attribute, else the
        2px browser default."""
        bs = node.style.get("border-spacing", "").strip()
        if bs:
            val = parse_size(bs.split()[0], 0, em)
            if val is not None:
                return max(val, 0.0)
        cs = node.attributes.get("cellspacing")
        if cs is not None:
            try:
                return max(float(str(cs).replace("px", "").strip()), 0.0)
            except (ValueError, TypeError):
                pass
        return 2.0

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
            # queue only out-of-flow *descendants*; self.node is the box
            # already being laid out (it was pulled off abs_queue), so its
            # own children must flow here instead of re-queuing it
            if node is not self.node and is_out_of_flow(node):
                line = self.children[-1] if self.children else None
                sy = line.y if line is not None else self.y
                self._queue_abs(node, self.x + self.cursor_x, sy)
                return
            _disp = node.style.get("display", "")
            if node is not self.node and (
                    node.tag == "input"
                    or (_disp in ("inline-block", "inline-flex",
                                  "inline-table")
                        and node.tag not in ("img", "svg", "br"))):
                # a descendant inline-block (or a replaced <input>) becomes
                # an atomic box; the container node itself (node is
                # self.node) falls through so its own children flow,
                # avoiding infinite re-entry.
                self.inline_block(node)
                return
            if node.tag == "br":
                self.new_line()
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
        # A run wider than the whole line can't fit however it wraps. Break
        # it between characters when the script allows: CJK/Hangul/Kana
        # always break between ideographs, and word-break:break-all /
        # overflow-wrap:break-word|anywhere break any long token (long
        # URLs, hashes). Otherwise it overflows as one word (as before).
        if not nowrap and w > self.width and self.width > 0:
            # word-break / overflow-wrap are inherited properties; this
            # engine doesn't inherit them, so read the nearest ancestor
            # that sets one (only reached for an over-wide word, so cheap)
            def _anc(prop):
                n = getattr(node, "parent", None)
                while n is not None:
                    v = n.style.get(prop) if hasattr(n, "style") else None
                    if v:
                        return v.strip().casefold()
                    n = getattr(n, "parent", None)
                return ""
            wb = _anc("word-break")
            ow = _anc("overflow-wrap") or _anc("word-wrap")
            force = wb == "break-all" or ow in ("break-word", "anywhere")
            if force or _has_cjk(word):
                self._emit_broken(node, word, font, cjk_only=not force)
                return
        if not nowrap and self.cursor_x + w > self.width \
                and self.cursor_x > 0:
            self.new_line()
        line = self.children[-1]
        prev = line.children[-1] if line.children else None
        text = TextLayout(node, word, line, prev)
        line.children.append(text)
        self.cursor_x += w + measure(font, " ")

    def _emit_broken(self, node, word, font, cjk_only):
        """Emit a too-wide run split at break opportunities, wrapping
        across lines. For cjk_only each CJK char is its own break point
        while maximal runs of other characters stay whole; otherwise
        every character may break. Segments carry no inter-segment space
        (CJK is written without spaces)."""
        if cjk_only:
            segs, buf = [], ""
            for ch in word:
                if _is_cjk(ch):
                    if buf:
                        segs.append(buf)
                        buf = ""
                    segs.append(ch)
                else:
                    buf += ch
            if buf:
                segs.append(buf)
        else:
            segs = list(word)
        for seg in segs:
            sw = measure(font, seg)
            if self.cursor_x + sw > self.width and self.cursor_x > 0:
                self.new_line()
            line = self.children[-1]
            prev = line.children[-1] if line.children else None
            line.children.append(
                TextLayout(node, seg, line, prev, keep_spaces=True))
            self.cursor_x += sw

    def inline_block(self, node):
        line = self.children[-1]
        prev = line.children[-1] if line.children else None
        box = InlineBlockLayout(node, line, prev, self)
        # wrap to a fresh line if it would overflow (unless already at the
        # line start — an over-wide box just overflows its own line)
        if self.cursor_x + box.width > self.width and self.cursor_x > 0:
            self.new_line()
            line = self.children[-1]
            box.parent = line
            box.previous = None
        line.children.append(box)
        self.cursor_x += box.width

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
                bg = self.node.style["background"]
                # a gradient's interior color tokens are stops, not the
                # box fill — scanning them tokenwise picked a later
                # gradient layer's colour and flooded the box (naver's
                # gradient-border banner filled the page over the news).
                # Defer any gradient to gradient_color's first-stop below.
                if "gradient(" not in bg.casefold():
                    for token in bg.split():
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
            # note: overflow:auto is intentionally NOT clipped here — in a
            # non-scrolling full-page render, clipping an auto scroll
            # container to a possibly under-computed height would hide
            # readable content, which is worse than letting it flow.
            if self.node.style.get(axis) in ("hidden", "clip", "scroll"):
                return True
        return False

    def paint_after(self):
        return [DrawClipPop()] if self._clips() else []


class InlineBlockLayout:
    """`display:inline-block` — an atomic box in the inline flow. Its own
    children lay out as a mini block (shrink-to-fit width unless a width is
    given); it sits on the line's baseline like a replaced element and
    wraps to the next line when it would overflow. This is what makes card
    and column grids (naver's feed, news items) flow side by side instead
    of stacking. The inner block is laid out once at the origin to measure,
    then re-anchored to its final (x, y) once the line assigns them."""

    def __init__(self, node, line, previous, container):
        self.node = node
        self.parent = line
        self.previous = previous
        self.font = None            # atomic: baseline-aligned like an image
        self.x = 0
        self.y = 0
        self.margin_top = 0
        self.margin_bottom = 0
        em = parse_px(node.style.get("font-size", "16px"), 16.0)
        avail = max(line.width, 1)
        w = parse_size(node.style.get("width"), avail, em)
        if w is None:
            try:
                w = _measure_content_width(node, container._document())
            except Exception:
                w = avail
            w = min(w, avail)
        inner = BlockLayout(node, container, None)
        inner.forced_width = max(w, 0.0)
        inner.flex_origin = (0, 0)
        inner.layout()
        self.inner = inner
        self.children = [inner]
        self.width = inner.outer_width()
        self.height = inner.outer_height()

    def layout(self):
        # x flows from the previous item on the line; y is set afterward by
        # the LineLayout (baseline), then place_inner() re-anchors the box.
        if self.previous:
            self.x = self.previous.x + self.previous.width
        else:
            self.x = self.parent.x

    def place_inner(self):
        self.inner.flex_origin = (self.x, self.y)
        self.inner.layout()

    def paint(self):
        return []


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

        # text-align — skipped during intrinsic-width measurement, where
        # the line box is _MAXCONTENT-wide and a right/center shift would
        # push the words out to that width and inflate the measured extent
        align = self.node.style.get("text-align", "left")
        try:
            measuring = self.parent._document()._measuring
        except Exception:
            measuring = False
        if not measuring and align in ("center", "right") and self.children:
            last = self.children[-1]
            used = (last.x + last.width) - self.x
            free = self.width - used
            if free > 0:
                shift = free / 2 if align == "center" else free
                for word in self.children:
                    word.x += shift

        # inline-block boxes re-anchor their inner block once x (incl. any
        # text-align shift) and the baseline y are final
        for word in self.children:
            place = getattr(word, "place_inner", None)
            if place is not None:
                place()

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
