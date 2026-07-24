"""Layout engine: builds a box tree from the styled DOM.

DocumentLayout -> BlockLayout (block or inline mode)
              -> LineLayout -> TextLayout (one word each)
"""

import re
import tkinter.font

from . import textengine
from .colors import NAMED
from .draw import (DrawBgImage, DrawClipPop, DrawClipPush, DrawImage,
                   DrawLine, DrawOval, DrawRect, DrawStickyPop,
                   DrawStickyPush, DrawText,
                   translate_cmds)
from .html_parser import Element, Text
from .style import parse_px, parse_size

# The initial containing block IS the viewport: html fills it edge to
# edge (x=0, full width). Any page gutter comes from the UA sheet's
# `body { margin: 8px }`, exactly like a real browser — a nonzero HSTEP
# here would double-inset and, worse, shrink the usable width so a page
# whose own centered container is sized to the full viewport (naver's
# `.container { width: 1280px; margin: auto }`) no longer fits and its
# right column spills past the edge.
HSTEP = 0
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


def line_height_px(node, font_px):
    """CSS line-height resolved to a used line-box height in px, or
    None for `normal` (the engine's default leading applies). A number
    multiplies the font size; a length is taken relative to it."""
    raw = node.style.get("line-height", "").strip().casefold()
    if not raw or raw == "normal":
        return None
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
        return None
    return target if target >= 0 else None


def line_height_factor(node, font_px):
    """CSS line-height as a multiple of the font size (`normal` keeps
    the engine's 1.25). Kept for callers that only know the font size;
    LineLayout itself resolves against real font metrics via
    line_height_px so `line-height: 20px` yields a 20px line box."""
    target = line_height_px(node, font_px)
    if target is None:
        return 1.25
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
    # a floated element is block-level (float computes display to block),
    # so `<p><img style="float:left">text</p>` puts the container in block
    # flow and the image actually floats instead of sitting inline
    if child.style.get("float", "none").strip().casefold() in (
            "left", "right"):
        return True
    d = child.style.get("display", "")
    if d in ("inline", "inline-block", "inline-flex", "inline-table"):
        return False
    # -webkit-box is the legacy flexbox used solely as the line-clamp
    # container (`display:block;display:-webkit-box` — last wins): it
    # must be a block-level box or an inline <strong class=title> never
    # gets its own box and the clamp/max-height are silently dropped
    if d in ("block", "flex", "grid", "table", "list-item",
             "-webkit-box"):
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
    if display in ("grid", "inline-grid") and node.children:
        return "grid"
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
    # flow rather than the line-based inline path. Synthesized ::before/
    # ::after boxes don't count: an inline-block pseudo (naver's 1px link
    # dividers, dot separators) must flow on the same line as the text
    # beside it, not push the text into a stacked block row.
    if any(isinstance(child, Element)
           and child.style.get("display", "") in (
               "inline-block", "inline-flex", "inline-table")
           and child.tag not in ("img", "svg", "br", "::before", "::after")
           for child in node.children):
        return "block"
    if node.tag in ("svg", "::before", "::after"):
        return "inline"  # replaced/synthesized: inline by nature
    if node.children or node.tag in ("br", "hr", "input", "img"):
        return "inline"
    return "block"


_CLIP_RECT_RE = re.compile(r"rect\(([^)]*)\)")


def _visually_hidden(node):
    """The screen-reader-only idiom used across Korean portals (Naver's
    `.blind`, etc.): a 1x1 box clipped to nothing that keeps text for
    assistive tech but must not paint. We don't clip sub-pixel boxes
    precisely, so treat the whole subtree as hidden. Kept deliberately
    narrow so real content and 1px dividers are never caught."""
    if not isinstance(node, Element):
        return False
    style = node.style
    clip = (style.get("clip") or "").strip().casefold()
    m = _CLIP_RECT_RE.search(clip)
    if m and "auto" not in m.group(1):
        nums = re.findall(r"-?\d*\.?\d+", m.group(1))
        # clip:rect(0 0 0 0) collapses the element to an empty region
        if nums and all(abs(float(v)) <= 1.0 for v in nums):
            return True

    def _tiny(value):
        v = (value or "").strip().casefold()
        if v.endswith("px"):
            try:
                return float(v[:-2]) <= 1.0
            except ValueError:
                return False
        return v in ("0",)

    if (style.get("overflow", "visible").strip().casefold() == "hidden"
            and _tiny(style.get("width")) and _tiny(style.get("height"))):
        return True
    return False


def is_visible(node):
    if node.style.get("display", "inline") == "none":
        return False
    return not _visually_hidden(node)


def collapse_margins(*values):
    """Collapse an adjoining vertical-margin set (CSS 2.1 §8.3.1)."""
    vals = [float(v) for v in values if v is not None]
    if not vals:
        return 0.0
    return max([0.0] + vals) + min([0.0] + vals)


def _node_margin(node, side, avail):
    if not isinstance(node, Element):
        return 0.0
    em = parse_px(node.style.get("font-size", "16px"), 16.0)
    return parse_size(node.style.get("margin-" + side), avail, em) or 0.0


def _node_edge_size(node, prop, avail):
    if not isinstance(node, Element):
        return 0.0
    em = parse_px(node.style.get("font-size", "16px"), 16.0)
    return parse_size(node.style.get(prop), avail, em) or 0.0


def _margin_context_allows_children(node):
    """Whether this node can collapse its block children's margins."""
    if not isinstance(node, Element) or node.tag in ("html", "body"):
        return False
    if layout_mode(node) != "block":
        return False
    style = node.style
    if style.get("overflow", "visible").strip().casefold() not in (
            "", "visible"):
        return False
    if style.get("float", "none").strip().casefold() in ("left", "right"):
        return False
    if style.get("position", "static").strip().casefold() in (
            "absolute", "fixed"):
        return False
    return style.get("display", "").strip().casefold() not in (
        "flow-root", "inline-block", "inline-flex", "inline-grid",
        "inline-table")


def _flow_edge_children(node, reverse=False):
    """Yield block children from one edge until inline content blocks it."""
    children = reversed(node.children) if reverse else iter(node.children)
    for child in children:
        if not is_visible(child):
            continue
        if isinstance(child, Text):
            if not child.text.strip():
                continue
            break
        if is_out_of_flow(child) or child.style.get(
                "float", "none").strip().casefold() in ("left", "right"):
            continue
        if not _child_is_block_level(child):
            break
        yield child


def _node_is_empty_collapsible(node, avail, depth=0):
    """Whether a normal-flow block's own top/bottom margins adjoin."""
    if depth > 64 or not _margin_context_allows_children(node):
        return False
    style = node.style
    for prop in ("padding-top", "padding-bottom", "border-width",
                 "border-top-width", "border-bottom-width"):
        if _node_edge_size(node, prop, avail) != 0:
            return False
    em = parse_px(style.get("font-size", "16px"), 16.0)
    height = parse_size(style.get("height"), 0, em)
    min_height = parse_size(style.get("min-height"), 0, em)
    if (height is not None and height != 0) \
            or (min_height is not None and min_height != 0):
        return False
    for child in node.children:
        if not is_visible(child):
            continue
        if isinstance(child, Text):
            if child.text.strip():
                return False
            continue
        if is_out_of_flow(child) or child.style.get(
                "float", "none").strip().casefold() in ("left", "right"):
            continue
        if not _child_is_block_level(child) \
                or not _node_is_empty_collapsible(child, avail, depth + 1):
            return False
    return True


def _collapsed_node_top_margin(node, avail, depth=0):
    """Effective top margin including collapsible descendant/empty chains."""
    own = _node_margin(node, "top", avail)
    if depth > 64 or not _margin_context_allows_children(node):
        return own
    if _node_edge_size(node, "padding-top", avail) != 0 \
            or _node_edge_size(node, "border-width", avail) != 0 \
            or _node_edge_size(node, "border-top-width", avail) != 0:
        return own
    margins = [own]
    for child in _flow_edge_children(node):
        if child.style.get("clear", "none").strip().casefold() != "none":
            break
        margins.append(_collapsed_node_top_margin(child, avail, depth + 1))
        if _node_is_empty_collapsible(child, avail, depth + 1):
            margins.append(_node_margin(child, "bottom", avail))
            continue
        break
    return collapse_margins(*margins)


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


def _list_marker_visible(node):
    """Whether an <li> paints its bullet. `list-style(-type): none`
    (checked up the chain — the property inherits and sites set it on
    the <ul>) and any display other than the default list box suppress
    the marker; nav/tab <li>s are inline-block and must stay clean."""
    disp = (node.style.get("display") or "").strip().casefold()
    if disp and disp not in ("list-item", "block"):
        return False
    n = node
    for _ in range(12):
        if not isinstance(n, Element):
            break
        for prop in ("list-style-type", "list-style"):
            v = (n.style.get(prop) or "").strip().casefold()
            if v:
                return "none" not in v
        n = getattr(n, "parent", None)
    return True


def _split_top_level(spec):
    """Split a track list on spaces while keeping parenthesised groups
    (minmax(...), repeat(...), fit-content(...)) intact. CSS Grid line-name
    groups are returned as their own tokens, including when authors omit
    whitespace (`[content-start]1fr`)."""
    out, buf, depth, bracket = [], "", 0, False
    for ch in spec:
        if ch == "[" and depth == 0 and not bracket:
            if buf:
                out.append(buf)
            buf = ch
            bracket = True
        elif ch == "]" and bracket:
            buf += ch
            out.append(buf)
            buf = ""
            bracket = False
        elif ch == "(" and not bracket:
            depth += 1
            buf += ch
        elif ch == ")" and not bracket:
            depth -= 1
            buf += ch
        elif ch.isspace() and depth == 0 and not bracket:
            if buf:
                out.append(buf)
                buf = ""
        else:
            buf += ch
    if buf:
        out.append(buf)
    return out


def _grid_track_size(token, avail, em):
    """One grid track -> ('fr', n) for flexible tracks or ('fixed', px)
    for a resolved length. auto/min-content/max-content and unresolved
    values become a flexible 1fr so the track still shares space rather
    than collapsing."""
    t = token.strip().casefold()
    if t.endswith("fr"):
        try:
            return ("fr", max(float(t[:-2]), 0.0))
        except ValueError:
            return ("fr", 1.0)
    if t.startswith("minmax(") and t.endswith(")"):
        parts = t[7:-1].split(",")
        if len(parts) == 2:
            return _grid_track_size(parts[1], avail, em)  # the max
    if t.startswith("fit-content(") and t.endswith(")"):
        return _grid_track_size(t[12:-1], avail, em)
    if t in ("auto", "min-content", "max-content", "fit-content"):
        return ("fr", 1.0)
    px = parse_size(token, avail, em)
    if px is not None:
        return ("fixed", max(px, 0.0))
    return ("fr", 1.0)


def _parse_aspect_ratio(value):
    """CSS aspect-ratio -> a width/height ratio (float), or None. Accepts
    '16/9', '1.5', or '16 / 9' (an 'auto' prefix/keyword is ignored)."""
    v = (value or "").strip()
    if not v:
        return None
    m = re.search(r"([0-9]*\.?[0-9]+)\s*(?:/\s*([0-9]*\.?[0-9]+))?", v)
    if not m:
        return None
    try:
        a = float(m.group(1))
        b = float(m.group(2)) if m.group(2) else 1.0
        return a / b if b else None
    except ValueError:
        return None


def _parse_grid_areas(spec):
    """grid-template-areas -> a list of rows, each a list of area names
    ('.' is an empty cell). Each quoted string is one row."""
    rows = []
    for m in re.finditer(r'"([^"]*)"|\'([^\']*)\'', spec or ""):
        s = m.group(1) if m.group(1) is not None else m.group(2)
        cells = s.split()
        if cells:
            rows.append(cells)
    return rows


def _parse_grid_template(spec, avail, em):
    """Parse a grid track template.

    Returns ``(tracks, line_names)`` where line names map to every matching
    zero-based grid-line index. Keeping all occurrences is important for
    ``repeat()`` and for placements such as ``item 2`` / ``span item``.
    """
    spec = (spec or "").strip()
    if not spec or spec in ("none", "auto"):
        return [], {}
    tracks, line_names = [], {}

    def add_parts(parts):
        for part in parts:
            p = part.strip()
            pl = p.casefold()
            if p.startswith("[") and p.endswith("]"):
                for name in p[1:-1].split():
                    if name:
                        line_names.setdefault(name, []).append(len(tracks))
                continue
            if pl.startswith("repeat(") and pl.endswith(")"):
                inner = p[7:-1]
                comma = inner.find(",")
                if comma < 0:
                    continue
                try:
                    count = int(inner[:comma].strip())
                except ValueError:
                    count = 1          # auto-fill/auto-fit: one pass
                count = max(1, min(count, 64))
                sub = _split_top_level(inner[comma + 1:].strip())
                for _ in range(count):
                    add_parts(sub)
                continue
            tracks.append(_grid_track_size(p, avail, em))

    add_parts(_split_top_level(spec))
    return tracks, line_names


def _parse_grid_tracks(spec, avail, em):
    """Compatibility helper for callers interested only in track sizes."""
    return _parse_grid_template(spec, avail, em)[0]


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


def _sticky_metrics(box):
    """Return ``(normal_margin_top, max_margin_top, top_inset)``.

    This engine has one document viewport and no independently scrolling
    element boxes, so the containing block is the laid-out parent. The
    bounds are computed after layout, when the parent's final height is
    known, and are shared by painting and hit-testing.
    """
    if not isinstance(box, BlockLayout) or not isinstance(box.node, Element):
        return None
    style = box.node.style
    if style.get("position", "static").strip().casefold() != "sticky":
        return None
    raw = style.get("top", "").strip().casefold()
    if not raw or raw == "auto":
        return None
    doc = box._document()
    em = parse_px(style.get("font-size", "16px"), 16.0)
    base = doc.viewport_height
    if base is None:
        base = getattr(box.parent, "height", 0.0)
    inset = parse_size(raw, base, em)
    if inset is None:
        return None
    normal = box.y - box.pt - box.bw - box.margin_top
    parent_bottom = (box.parent.y + box.parent.height
                     + getattr(box.parent, "pb", 0.0))
    maximum = max(normal, parent_bottom - box.outer_height())
    return normal, maximum, inset


def sticky_offset(layout_obj, scroll):
    """Current vertical paint offset for a box inside sticky ancestors."""
    total = 0.0
    chain = []
    cur = layout_obj
    while cur is not None:
        chain.append(cur)
        cur = getattr(cur, "parent", None)
    for box in reversed(chain):
        metrics = _sticky_metrics(box)
        if metrics is None:
            continue
        normal, maximum, inset = metrics
        shifted_normal = normal + total
        shifted_maximum = maximum + total
        stuck = min(max(shifted_normal, scroll + inset), shifted_maximum)
        total += stuck - shifted_normal
    return total


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

            st = node.style
            # position:fixed is laid out relative to the viewport, not a
            # positioned ancestor or the whole (tall) page — so top:0 pins
            # to the top of the first screen and bottom:0 to the bottom of
            # the viewport (best a non-scrolling full-page render can do).
            if st.get("position") == "fixed":
                cb_x, cb_y = 0.0, 0.0
                cb_w = self.viewport_width or self.width
                cb_h = self.viewport_height or self.height
            elif isinstance(cb, BlockLayout):
                # containing-block padding box
                cb_x = cb.x - cb.pl
                cb_y = cb.y - cb.pt
                cb_w = cb.width + cb.pl + cb.pr
                cb_h = cb.height + cb.pt + cb.pb
            else:                     # document / initial containing block
                cb_x, cb_y, cb_w, cb_h = self.x, self.y, self.width, \
                    self.height

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

            # CSS 2.1 §10.3.7: an absolutely-positioned box's auto
            # horizontal margins resolve against its CONTAINING BLOCK, not
            # the document. box.layout() sized them against the document
            # (self.width), so `margin-left:auto` on an icon inside a
            # 36px button ballooned to ~1200px and shoved the icon off to
            # the right. Recompute them here; the translate below is
            # margin-aware, so fixing box.ml/mr repositions correctly.
            ml_auto = (st.get("margin-left") or "").strip() == "auto"
            mr_auto = (st.get("margin-right") or "").strip() == "auto"
            if ml_auto or mr_auto:
                border_box_w = (box.bw * 2 + box.pl + box.width + box.pr)
                if left is not None and right is not None:
                    leftover = max(cb_w - left - right - border_box_w, 0.0)
                else:
                    leftover = 0.0  # under-constrained: auto margins are 0
                if ml_auto and mr_auto:
                    box.ml = box.mr = leftover / 2
                elif ml_auto:
                    box.ml = leftover
                else:
                    box.mr = leftover

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
        self._collapsed_top_children = set()
        self._margin_through = False
        self._through_anchor = 0.0
        self._through_margins = ()

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
        self._collapsed_top_children = set()
        self._margin_through = False
        self._through_margins = ()
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

        mode = layout_mode(node)
        if _margin_context_allows_children(node) \
                and self.pt == 0 and self.bw == 0:
            adjoining = [self.margin_top]
            for child in _flow_edge_children(node):
                if child.style.get(
                        "clear", "none").strip().casefold() != "none":
                    break
                adjoining.append(_collapsed_node_top_margin(child, avail))
                self._collapsed_top_children.add(child)
                if _node_is_empty_collapsible(child, avail):
                    adjoining.append(_node_margin(child, "bottom", avail))
                    continue
                break
            self.margin_top = collapse_margins(*adjoining)

        edge = self.pl + self.pr + 2 * self.bw
        spec = size("width")
        maxw = size("max-width")
        minw = size("min-width")
        # box-sizing (CSS Box Sizing §3): border-box counts padding+border
        # inside a specified width, content-box adds them outside. This
        # engine defaults a bare width to border-box ("web reality": most
        # sites ship `* { box-sizing: border-box }`), and honours an
        # explicit box-sizing:content-box for the pages that opt out.
        border_box = st.get("box-sizing", "").strip().casefold() \
            != "content-box"

        def to_border(v):
            return v if border_box else v + edge
        if self.forced_width is not None:
            box_w = self.forced_width
        elif spec is not None:
            box_w = to_border(spec)
        else:
            box_w = avail - self.ml - self.mr
        if maxw is not None:
            box_w = min(box_w, to_border(maxw))
        if minw is not None:
            box_w = max(box_w, to_border(minw))
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

        collapsed_with_parent = node in getattr(
            self.parent, "_collapsed_top_children", ())
        if self.flex_origin is not None:
            base_x, base_y = self.flex_origin
            self.x = base_x + self.ml + self.bw + self.pl
            self.y = base_y + self.margin_top + self.bw + self.pt
        else:
            self.x = self.parent.x + self.ml + self.bw + self.pl
            if collapsed_with_parent:
                self.y = self.parent.y + self.bw + self.pt
            elif self.previous:
                p = self.previous
                # CSS 2.1 §8.3.1: adjacent vertical margins collapse. The
                # gap between two in-flow block siblings is a single
                # margin, not the sum — max of the positive parts plus the
                # min of the negative parts (so equal 16px margins give a
                # 16px gap, not 32px).
                if getattr(p, "_margin_through", False):
                    collapse = collapse_margins(
                        *p._through_margins, self.margin_top)
                    self.y = (p._through_anchor + collapse
                              + self.bw + self.pt)
                else:
                    collapse = collapse_margins(
                        p.margin_bottom, self.margin_top)
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
        # can resolve percentage heights against it. box-sizing:border-box
        # counts padding+border inside the height; content-box (default)
        # is already the content height.
        spec_h = self._specified_height(st, em)
        if spec_h is not None:
            self.definite_height = max(
                spec_h - (self.pt + self.pb + 2 * self.bw)
                if border_box else spec_h, 0)

        if mode == "flex":
            self._layout_flex(node, em)
        elif mode == "table":
            self._layout_table(node, em)
        elif mode == "grid":
            self._layout_grid(node, em)
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
                # Formatting whitespace between block tags creates no box
                # and must not interrupt adjoining sibling/parent margins.
                if isinstance(child, Text) and not child.text.strip():
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
                        if fw is None:
                            # auto-width float: shrink-to-fit, capped to the
                            # containing block (floated <img>/<figure> size
                            # to their content instead of stacking as blocks)
                            try:
                                fw = min(
                                    _measure_content_width(
                                        child, self._document()),
                                    self.width)
                            except Exception:
                                fw = self.width
                            fw = max(fw, 0.0)
                if fside in ("left", "right"):
                    # float: out of normal flow, anchored where the flow
                    # currently ends
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
            can_collapse_bottom = (
                _margin_context_allows_children(node)
                and self.pb == 0 and self.bw == 0
                and spec_h is None
                and (_node_edge_size(node, "min-height", avail) == 0)
                and isinstance(previous, BlockLayout))
            if can_collapse_bottom:
                if previous._margin_through:
                    self.margin_bottom = collapse_margins(
                        self.margin_bottom, *previous._through_margins)
                    flow_bottom = previous._through_anchor
                else:
                    self.margin_bottom = collapse_margins(
                        self.margin_bottom, previous.margin_bottom)
                    flow_bottom = (previous.y + previous.height
                                   + previous.pb + previous.bw)
            else:
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
            self._ws_pending = False   # no space owed before the first atom
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

        # A zero-height block with no border/padding has adjoining top and
        # bottom margins. Preserve the whole set and the preceding border
        # edge so the next sibling collapses across the empty box instead
        # of paying two separate gaps.
        if self.height == 0 and _node_is_empty_collapsible(node, avail):
            self._margin_through = True
            if collapsed_with_parent:
                self._through_anchor = self.parent.y
                self._through_margins = ()
            elif getattr(self.previous, "_margin_through", False):
                self._through_anchor = self.previous._through_anchor
                self._through_margins = (
                    *self.previous._through_margins,
                    self.margin_top, self.margin_bottom)
            elif self.previous is not None:
                self._through_anchor = (
                    self.previous.y + self.previous.height
                    + self.previous.pb + self.previous.bw)
                self._through_margins = (
                    self.previous.margin_bottom,
                    self.margin_top, self.margin_bottom)
            else:
                self._through_anchor = self.parent.y
                self._through_margins = (
                    self.margin_top, self.margin_bottom)

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

    def _layout_flex_column(self, node, em, kids, row_gap):
        """flex-direction:column — the flex algorithm on a vertical main
        axis. Item base sizes come from layout; a definite container
        height is handed out by flex-grow / removed by flex-shrink;
        justify-content distributes leftover space down the column and
        align-items/align-self place items across it (stretch fills the
        width, center/end shrink-to-fit and shift). row-gap sits between
        items."""
        doc = self._document()
        align = node.style.get("align-items", "stretch").strip().casefold()

        def _num(v, default):
            try:
                return max(float(v), 0.0)
            except (TypeError, ValueError):
                return default

        boxes, aligns = [], []
        for child in kids:
            a = child.style.get("align-self", "").strip().casefold() or align
            aligns.append(a)
            box = BlockLayout(child, self, None)
            if a in ("stretch", "normal", "auto", ""):
                box.forced_width = self.width          # fill the cross axis
            elif not child.style.get("width"):
                # non-stretch, auto width: shrink-to-fit so it can be
                # centred/end-aligned (an explicit width is left to the box)
                try:
                    cw = _measure_content_width(child, doc)
                except Exception:
                    cw = self.width
                box.forced_width = max(min(cw, self.width), 0.0)
            box.flex_origin = (self.x, self.y)
            box.layout()
            boxes.append(box)
            self.children.append(box)

        n = len(boxes)
        heights = [b.outer_height() for b in boxes]
        gap_total = row_gap * max(n - 1, 0)

        # grow / shrink against a definite container height
        if self.definite_height is not None:
            free = self.definite_height - sum(heights) - gap_total
            if free > 0:
                grows = [_num(c.style.get("flex-grow"), 0.0) for c in kids]
                tg = sum(grows)
                if tg > 0:
                    for i, g in enumerate(grows):
                        add = free * g / tg
                        heights[i] += add
                        boxes[i].height += add
            elif free < 0:
                shrinks = [_num(c.style.get("flex-shrink"), 1.0)
                           for c in kids]
                ws = [shrinks[i] * heights[i] for i in range(n)]
                wsum = sum(ws)
                if wsum > 0:
                    for i in range(n):
                        take = min(-free * ws[i] / wsum, heights[i])
                        heights[i] -= take
                        boxes[i].height = max(boxes[i].height - take, 0.0)

        # justify-content down the main axis
        total = sum(heights) + gap_total
        free_v = max(self.definite_height - total, 0.0) \
            if self.definite_height is not None else 0.0
        justify = node.style.get(
            "justify-content", "flex-start").strip().casefold()
        lead, gap_extra = 0.0, 0.0
        if free_v > 0 and n > 0:
            if justify == "center":
                lead = free_v / 2
            elif justify in ("flex-end", "end"):
                lead = free_v
            elif justify == "space-between":
                gap_extra = free_v / (n - 1) if n > 1 else 0.0
            elif justify == "space-around":
                gap_extra = free_v / n
                lead = gap_extra / 2
            elif justify == "space-evenly":
                gap_extra = free_v / (n + 1)
                lead = gap_extra

        y = self.y + lead
        for i, b in enumerate(boxes):
            translate(b, 0, y - (b.y - b.margin_top - b.bw - b.pt))
            a = aligns[i]
            if a == "center":
                dx = (self.width - b.outer_width()) / 2
                if dx > 0:
                    translate(b, dx, 0)
            elif a in ("flex-end", "end"):
                dx = self.width - b.outer_width()
                if dx > 0:
                    translate(b, dx, 0)
            y += heights[i] + row_gap + gap_extra

        self.height = self.definite_height \
            if self.definite_height is not None else total
        apply_relative_offsets(self.children)

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
            self._layout_flex_column(node, em, kid_nodes, row_gap)
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
        n = len(specs)
        if not wrap:
            free = self.width - sum(specs) - gap_total
            if free > 0 and eff_total > 0:
                # grow: distribute positive free space, clamp each item to
                # its max-width, freeze it, and redistribute the remainder
                maxes = [parse_size(c.style.get("max-width"), self.width, em)
                         for c in kid_nodes]
                frozen = [eff_grows[i] <= 0 for i in range(n)]
                rem = free
                for _ in range(n + 1):
                    act = [i for i in range(n) if not frozen[i]]
                    tg = sum(eff_grows[i] for i in act)
                    if not act or rem <= 0.5 or tg <= 0:
                        break
                    hit = False
                    for i in act:
                        add = rem * eff_grows[i] / tg
                        if maxes[i] is not None \
                                and specs[i] + add > maxes[i]:
                            rem -= (maxes[i] - specs[i])
                            specs[i] = maxes[i]
                            frozen[i] = True
                            hit = True
                    if not hit:
                        for i in act:
                            specs[i] += rem * eff_grows[i] / tg
                        break
            elif free < 0:
                # shrink: remove overflow in proportion to shrink-factor x
                # base, but never below an item's min-content width (so text
                # can't be crushed into overlap); freeze and redistribute
                mins = [_measure_min_width(c, doc) for c in kid_nodes]
                frozen = [shrinks[i] <= 0 for i in range(n)]
                need = -free
                for _ in range(n + 1):
                    act = [i for i in range(n)
                           if not frozen[i] and specs[i] > mins[i]]
                    w = [shrinks[i] * specs[i] for i in act]
                    wsum = sum(w)
                    if not act or need <= 0.5 or wsum <= 0:
                        break
                    hit = False
                    for k, i in enumerate(act):
                        take = need * w[k] / wsum
                        if specs[i] - take < mins[i]:
                            need -= (specs[i] - mins[i])
                            specs[i] = mins[i]
                            frozen[i] = True
                            hit = True
                    if not hit:
                        for k, i in enumerate(act):
                            specs[i] = max(specs[i] - need * w[k] / wsum, 0.0)
                        break
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
                    if dy > 0:
                        translate(b, 0, dy)
                elif mode in ("flex-end", "end"):
                    dy = height - b.outer_height()
                    if dy > 0:
                        translate(b, 0, dy)
                elif mode in ("stretch", "normal", "auto", "") \
                        and b.definite_height is None:
                    # stretch (the default): an auto-height item grows to
                    # the line's cross size, so a row of cards ends up
                    # equal height (their backgrounds/borders fill)
                    fill = height - b.margin_top - b.margin_bottom \
                        - 2 * b.bw - b.pt - b.pb
                    if fill > b.height:
                        b.height = fill
                # flex-start / baseline: top edge, no change
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

    def _layout_grid(self, node, em):
        """CSS Grid with fixed/fr tracks, named lines and two-axis spans.
        Explicit placements reserve an occupancy grid before row-major
        auto-placement; row heights come from fixed tracks and content.
        This turns the
        common 2–3 column app shells (Wikipedia's Vector sidebar/content,
        card grids) into real columns instead of a single stacked block."""
        doc = self._document()
        style = node.style

        cols_spec = style.get("grid-template-columns", "")
        rows_spec = style.get("grid-template-rows", "")
        if not cols_spec:
            shorthand = style.get("grid-template") or style.get("grid") or ""
            if "/" in shorthand:          # "<rows> / <columns>"
                cols_spec = shorthand.split("/", 1)[1]
                if not rows_spec and '"' not in shorthand \
                        and "'" not in shorthand:
                    rows_spec = shorthand.split("/", 1)[0]
        if "/" in cols_spec:              # a shorthand leaked into the key
            cols_spec = cols_spec.split("/", 1)[1]
        tracks, col_lines = _parse_grid_template(cols_spec, self.width, em)
        if not tracks:
            tracks = [("fr", 1.0)]
        ncols = len(tracks)
        row_tracks, row_lines = _parse_grid_template(
            rows_spec, self.definite_height or self.width, em)

        col_gap = parse_size(
            style.get("column-gap") or style.get("grid-column-gap")
            or self._gap_shorthand(style, 1), self.width, em) or 0.0
        row_gap = parse_size(
            style.get("row-gap") or style.get("grid-row-gap")
            or self._gap_shorthand(style, 0), self.width, em) or 0.0

        # resolve column widths: fixed tracks keep their px, fr tracks
        # split the remaining space
        gap_total = col_gap * max(ncols - 1, 0)
        fixed = sum(v for kind, v in ((t[0], t[1] if len(t) > 1 else 0.0)
                                      for t in tracks) if kind == "fixed")
        fr_sum = sum(t[1] for t in tracks if t[0] == "fr")
        free = max(self.width - gap_total - fixed, 0.0)
        col_w = []
        for t in tracks:
            if t[0] == "fixed":
                col_w.append(t[1])
            else:
                col_w.append(free * t[1] / fr_sum if fr_sum > 0 else 0.0)
        col_x = []
        cx = 0.0
        for i in range(ncols):
            col_x.append(cx)
            cx += col_w[i] + col_gap

        items = []
        for child in node.children:
            if not (isinstance(child, Element) and is_visible(child)):
                continue
            if is_out_of_flow(child):
                self._queue_abs(child, self.x, self.y)
                continue
            items.append(child)

        # placement: [child, col_start, col_span, row_start, row_span]
        areas = _parse_grid_areas(style.get("grid-template-areas", ""))
        name_box = {}
        if areas:
            # named areas: an item's grid-area name maps to the bounding
            # box of the cells carrying that name
            for r, row in enumerate(areas):
                for c, name in enumerate(row):
                    if not name or name == "." or c >= ncols:
                        continue
                    if name in name_box:
                        r0, r1, c0, c1 = name_box[name]
                        name_box[name] = (min(r0, r), max(r1, r),
                                          min(c0, c), max(c1, c))
                    else:
                        name_box[name] = (r, r, c, c)
            # Template areas create implicit <area>-start/end named lines.
            for name, (r0, r1, c0, c1) in name_box.items():
                col_lines.setdefault(name + "-start", []).append(c0)
                col_lines.setdefault(name + "-end", []).append(c1 + 1)
                row_lines.setdefault(name + "-start", []).append(r0)
                row_lines.setdefault(name + "-end", []).append(r1 + 1)

        specs = []
        for child in items:
            ga = child.style.get("grid-area", "").strip()
            if ga in name_box:
                r0, r1, c0, c1 = name_box[ga]
                specs.append([child, c0, c1 - c0 + 1,
                              r0, r1 - r0 + 1])
                continue
            c0, cs = self._grid_axis(child, "column", ncols, col_lines)
            row_count = max(len(row_tracks), len(areas), 1)
            r0, rs = self._grid_axis(child, "row", row_count, row_lines)
            specs.append([child, c0, cs, r0, rs])

        occupied = set()
        placed_by_index = [None] * len(specs)

        def normalise(spec):
            child, c0, cs, r0, rs = spec
            cs = max(1, min(cs, ncols))
            if c0 is not None:
                c0 = max(0, min(c0, ncols - cs))
            if r0 is not None:
                r0 = max(0, r0)
            return child, c0, cs, r0, max(1, rs)

        specs = [normalise(s) for s in specs]

        def fits(r0, c0, cs, rs):
            return all((r, c) not in occupied
                       for r in range(r0, r0 + rs)
                       for c in range(c0, c0 + cs))

        def reserve(index, child, c0, cs, r0, rs):
            placed_by_index[index] = [child, c0, cs, r0, rs]
            for r in range(r0, r0 + rs):
                for c in range(c0, c0 + cs):
                    occupied.add((r, c))

        # Explicit placements reserve their cells before source-ordered
        # auto placement, including when the explicit item appears later.
        for i, (child, c0, cs, r0, rs) in enumerate(specs):
            if c0 is not None and r0 is not None:
                reserve(i, child, c0, cs, r0, rs)

        cursor_r = cursor_c = 0
        for i, (child, c0, cs, r0, rs) in enumerate(specs):
            if placed_by_index[i] is not None:
                continue
            if c0 is not None:            # fixed column, find a free row
                r = 0
                while not fits(r, c0, cs, rs):
                    r += 1
                reserve(i, child, c0, cs, r, rs)
                continue
            if r0 is not None:            # fixed row, find a free column
                found = None
                for c in range(0, ncols - cs + 1):
                    if fits(r0, c, cs, rs):
                        found = c
                        break
                # Full Grid can create implicit columns here. This engine
                # keeps the explicit width and overlaps only as a fallback.
                reserve(i, child, found if found is not None else 0,
                        cs, r0, rs)
                continue

            # Fully automatic row-major placement.
            r, c = cursor_r, cursor_c
            while True:
                if c + cs > ncols:
                    r, c = r + 1, 0
                    continue
                if fits(r, c, cs, rs):
                    break
                c += 1
            reserve(i, child, c, cs, r, rs)
            cursor_r, cursor_c = r, c + cs
            if cursor_c >= ncols:
                cursor_r, cursor_c = r + 1, 0

        placed = [p for p in placed_by_index if p is not None]
        nrows = max(len(row_tracks), len(areas),
                    max((p[3] + p[4] for p in placed), default=0))
        row_h = [0.0] * nrows
        for i, track in enumerate(row_tracks):
            if track[0] == "fixed":
                row_h[i] = track[1]
        for p in placed:
            child, c0, cspan, r0, rspan = p
            w = sum(col_w[c0:c0 + cspan]) + col_gap * (cspan - 1)
            box = BlockLayout(child, self, None)
            box.forced_width = max(w, 0.0)
            box.flex_origin = (0.0, 0.0)
            box.layout()
            p.append(box)
            if rspan == 1:
                row_h[r0] = max(row_h[r0], box.outer_height())
        for p in placed:                 # multi-row items grow their last row
            child, c0, cspan, r0, rspan, box = p
            if rspan > 1:
                have = sum(row_h[r0:r0 + rspan]) + row_gap * (rspan - 1)
                if box.outer_height() > have:
                    row_h[r0 + rspan - 1] += box.outer_height() - have

        row_y = [0.0] * nrows
        y = 0.0
        for r in range(nrows):
            row_y[r] = y
            y += row_h[r] + row_gap
        total_h = (y - row_gap) if nrows else 0.0

        self.children = []
        for child, c0, cspan, r0, rspan, box in placed:
            box.flex_origin = (self.x + col_x[c0], self.y + row_y[r0])
            box.layout()
            self.children.append(box)
        self.height = max(total_h, 0.0)
        apply_relative_offsets(self.children)

    def _gap_shorthand(self, style, index):
        """Value from the `gap` shorthand: index 0 = row, 1 = column
        (one value applies to both)."""
        parts = style.get("gap", "").split()
        if not parts:
            return ""
        return parts[index] if index < len(parts) else parts[0]

    def _grid_axis(self, child, axis, track_count, line_names):
        """Return ``(start, span)`` for a grid row or column.

        Lines are zero-based internally. Positive/negative CSS line
        numbers, repeated named lines, ``span N`` / ``span <name>``,
        longhands, and the four-part ``grid-area`` shorthand are accepted.
        """
        prop = "grid-" + axis
        gc = child.style.get(prop, "").strip()
        start_raw = end_raw = ""
        if gc:
            if "/" in gc:
                start_raw, end_raw = (s.strip() for s in gc.split("/", 1))
            else:
                start_raw = gc
        area = [v.strip() for v in
                child.style.get("grid-area", "").split("/")]
        if len(area) > 1:
            ai = 1 if axis == "column" else 0
            ei = 3 if axis == "column" else 2
            if not start_raw and ai < len(area):
                start_raw = area[ai]
            if not end_raw and ei < len(area):
                end_raw = area[ei]
        start_raw = child.style.get(prop + "-start", "") or start_raw
        end_raw = child.style.get(prop + "-end", "") or end_raw

        def integer(v):
            try:
                return int(v)
            except (ValueError, TypeError):
                return None

        def parse_ref(raw):
            tokens = (raw or "").split()
            if not tokens or tokens[0].casefold() == "auto":
                return None
            is_span = tokens[0].casefold() == "span"
            if is_span:
                tokens = tokens[1:]
            number = None
            name = None
            for token in tokens:
                n = integer(token)
                if n is not None:
                    number = n
                elif token.casefold() != "auto":
                    name = token
            return is_span, name, number

        def numbered_line(number):
            if number is None or number == 0:
                return None
            # There are track_count + 1 lines; -1 is the final line.
            return number - 1 if number > 0 else track_count + 1 + number

        def named_line(name, occurrence=1, after=None, before=None):
            positions = list(line_names.get(name, ()))
            if after is not None:
                positions = [p for p in positions if p > after]
            if before is not None:
                positions = [p for p in positions if p < before]
            if not positions:
                return None
            occurrence = occurrence or 1
            if occurrence > 0:
                i = min(occurrence - 1, len(positions) - 1)
            else:
                i = max(-len(positions), occurrence)
            return positions[i]

        def absolute(ref, role, anchor=None):
            if ref is None or ref[0]:
                return None
            _span, name, number = ref
            if name is not None:
                # An unqualified named end searches forward from its start.
                after = anchor if role == "end" and number is None else None
                return named_line(name, number or 1, after=after)
            return numbered_line(number)

        start_ref, end_ref = parse_ref(start_raw), parse_ref(end_raw)
        s = absolute(start_ref, "start")
        e = absolute(end_ref, "end", s)

        if start_ref is not None and start_ref[0] and e is not None:
            _span, name, number = start_ref
            if name is not None:
                s = named_line(name, number or -1, before=e)
            else:
                s = e - max(number or 1, 1)
        if end_ref is not None and end_ref[0] and s is not None:
            _span, name, number = end_ref
            if name is not None:
                e = named_line(name, number or 1, after=s)
            else:
                e = s + max(number or 1, 1)

        if s is not None and e is not None:
            if e < s:
                s, e = e, s
            return s, max(e - s, 1)
        if s is not None:
            return s, 1
        if e is not None:
            return e - 1, 1
        # A span without an anchor still informs auto-placement width.
        for ref in (start_ref, end_ref):
            if ref is not None and ref[0] and ref[1] is None:
                return None, max(ref[2] or 1, 1)
        return None, 1

    def _grid_column(self, child, ncols):
        """Backward-compatible column placement helper."""
        return self._grid_axis(child, "column", ncols, {})

    # ----- inline layout -----

    def new_line(self):
        self.cursor_x = 0
        last = self.children[-1] if self.children else None
        self.children.append(LineLayout(self.node, self, last))

    def recurse(self, node):
        if isinstance(node, Text):
            ws = node.style.get("white-space", "normal")
            if ws == "pre":
                self.preformatted(node)
            elif ws in ("pre-wrap", "pre-line"):
                # both honor source newlines and still wrap; pre-line
                # collapses runs of whitespace, pre-wrap preserves them
                self.pre_wrapped(node, collapse=(ws == "pre-line"))
            else:
                # collapse whitespace, but remember whether the source had
                # any at each boundary so a space is inserted only where one
                # existed: "<b>a</b><b>b</b>" -> "ab", "$<b>5</b>" -> "$5",
                # while "hello <b>world</b>" keeps its space.
                text = node.text
                toks = text.split()
                if not toks:
                    if text:            # whitespace-only node: owes a space
                        self._ws_pending = True
                else:
                    lead = text[:1].isspace()
                    for i, word in enumerate(toks):
                        sb = (i > 0) or lead \
                            or getattr(self, "_ws_pending", False)
                        self.word(node, word, space_before=sb)
                        self._ws_pending = False
                    self._ws_pending = text[-1:].isspace()
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
            elif node.tag in ("::before", "::after") \
                    and node is not self.node:
                # synthesized icon: a fixed-size inline box painted by
                # its background layer; text content flows normally.
                # Never for self.node: an absolutely-positioned pseudo
                # laid out from the abs queue recurses into itself for
                # its children — re-adding it as an icon box here would
                # paint the sprite twice at offset positions (every
                # naver shortcut icon showed doubled)
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

    def word(self, node, word, space_before=True):
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
        line = self.children[-1]
        # a leading space only when the source had whitespace here and we
        # are not at the start of a line
        sp = measure(font, " ") if (space_before and line.children) else 0.0
        if not nowrap and self.cursor_x + sp + w > self.width \
                and self.cursor_x > 0:
            self.new_line()
            sp = 0.0
        line = self.children[-1]
        prev = line.children[-1] if line.children else None
        text = TextLayout(node, word, line, prev, keep_spaces=not space_before)
        line.children.append(text)
        self.cursor_x += sp + w

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
        # a CSS aspect-ratio overrides the natural ratio when deriving the
        # missing dimension (responsive width:100%;aspect-ratio:16/9 media)
        ar = _parse_aspect_ratio(node.style.get("aspect-ratio"))
        if w and not h:
            h = w / ar if ar else (
                w * natural_h / natural_w if natural_w else w)
        elif h and not w:
            w = h * ar if ar else (
                h * natural_w / natural_h if natural_h else h)
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

    def pre_wrapped(self, node, collapse):
        """white-space: pre-wrap / pre-line. Each source newline forces a
        break and long lines still wrap. pre-line collapses runs of
        whitespace (emit split words); pre-wrap preserves them (emit
        whitespace and word tokens verbatim, breaking between them)."""
        font = cached_font(node)
        for i, raw in enumerate(node.text.split("\n")):
            if i > 0:
                self.new_line()
            if collapse:
                for word in raw.split():
                    self.word(node, word)
                continue
            for token in re.findall(r"\s+|\S+", raw):
                w = measure(font, token)
                if token.strip() and self.cursor_x + w > self.width \
                        and self.cursor_x > 0:
                    self.new_line()
                line = self.children[-1]
                prev = line.children[-1] if line.children else None
                line.children.append(
                    TextLayout(node, token, line, prev, keep_spaces=True))
                self.cursor_x += w

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

            if self.node.tag == "li" and _list_marker_visible(self.node):
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
        # the line box has not been laid out yet (its width is still 0), so
        # resolve percentage widths against the container's content width —
        # e.g. width:100% on an inline <input> fills its container
        avail = max(getattr(container, "width", 0) or line.width, 1)
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
        max_descent = max(descent(w) for w in self.children)
        natural = max_ascent + max_descent
        target = line_height_px(self.node, font_px)
        if target is not None and natural > 0:
            # an explicit line-height IS the line-box height (leading is
            # distributed, glyphs may poke out when it is tighter than
            # the font) — naver clamps 2×20px titles into 40px, so a
            # 20/14×linespace box would spill past max-height and get
            # its glyphs clipped mid-height. Atomic boxes (icons,
            # inline-blocks) taller than the target still grow the line.
            atomic_max = max(
                (c.height for c in self.children if c.font is None),
                default=0.0)
            factor = max(target, atomic_max) / natural
        else:
            factor = 1.25
        baseline = self.y + factor * max_ascent
        self.height = factor * natural
        line_bottom = self.y + self.height

        # vertical-align (CSS 2.1 §10.8): offset each inline box from the
        # baseline. Default keeps the old baseline alignment; middle/top/
        # bottom/sub/super/text-top/text-bottom move icons and scripts to
        # the right height instead of all sitting on the baseline.
        def valign(child):
            n = child.node
            src = n if isinstance(n, Element) else getattr(n, "parent", None)
            v = (src.style.get("vertical-align", "")
                 if isinstance(src, Element) else "")
            return (v or "baseline").strip().casefold()

        xhalf = font_px * 0.25          # ~ half the x-height
        for word in self.children:
            asc = ascent(word)
            h = word.height
            va = valign(word)
            if va == "middle":
                word.y = baseline - xhalf - h / 2
            elif va == "top":
                word.y = self.y
            elif va == "bottom":
                word.y = line_bottom - h
            elif va == "text-top":
                word.y = baseline - max_ascent
            elif va == "text-bottom":
                word.y = baseline + max_descent - h
            elif va == "sub":
                word.y = baseline - asc + font_px * 0.15
            elif va == "super":
                word.y = baseline - asc - font_px * 0.30
            else:
                word.y = baseline - asc

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
        # inline-element backgrounds: an inline <span>/<a>/<mark> with a
        # background-color never gets its own box, so its highlight was
        # lost. Fill the rect its atoms span on this line, behind the
        # glyphs (LineLayout.paint runs before the word children paint).
        cmds = []
        groups = {}          # id(el) -> [el, min_x, max_x]
        block = self.node
        for child in self.children:
            n = getattr(child, "node", None)
            # atomic inline-block boxes paint their own background via
            # their inner block (a 1px divider pseudo would otherwise be
            # re-filled across its margin box and the whole line height);
            # only ancestor backgrounds apply to them here
            if isinstance(n, Text) or hasattr(child, "place_inner"):
                anc = getattr(n, "parent", None)
            else:
                anc = n
            x0 = child.x
            x1 = child.x + getattr(child, "width", 0)
            while isinstance(anc, Element) and anc is not block:
                bg = anc.style.get("background-color", "")
                if bg and safe_color(bg, default=""):
                    g = groups.get(id(anc))
                    if g is None:
                        groups[id(anc)] = [anc, x0, x1]
                    else:
                        g[1] = min(g[1], x0)
                        g[2] = max(g[2], x1)
                anc = getattr(anc, "parent", None)
        for el, x0, x1 in groups.values():
            if effective_opacity(el) < 0.05:
                continue
            color = safe_color(el.style.get("background-color"), default="")
            if color and x1 > x0:
                cmds.append(DrawRect(x0, self.y, x1, self.y + self.height,
                                     color))
        return cmds


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

    def _object_position(self, free_x, free_y):
        """object-position -> (dx, dy) offset of the scaled image inside
        its box, given the free space on each axis. Defaults to centered
        (50% 50%); supports keywords, %, and lengths."""
        val = self.node.style.get("object-position", "").strip().casefold()

        def axis(tokens, i, free):
            if len(tokens) > i:
                t = tokens[i]
                if t in ("left", "top"):
                    return 0.0
                if t in ("right", "bottom"):
                    return free
                if t == "center":
                    return free / 2
                if t.endswith("%"):
                    try:
                        return free * float(t[:-1]) / 100.0
                    except ValueError:
                        pass
                px = parse_size(t, 0)
                if px is not None:
                    return px
            return free / 2
        toks = val.split() if val else []
        return axis(toks, 0, free_x), axis(toks, 1, free_y)

    def paint(self):
        if effective_opacity(self.node) < 0.05:
            return []
        img = getattr(self.node, "_img", None)
        if img:
            nw, nh = img[1], img[2]
            bw, bh = self.width, self.height
            fit = self.node.style.get(
                "object-fit", "fill").strip().casefold()
            if fit in ("fill", "") or not (nw and nh) or bw <= 0 or bh <= 0:
                return [DrawImage(self.x, self.y, bw, bh, img[0])]
            # scale the natural size into the box per object-fit
            sx, sy = bw / nw, bh / nh
            if fit == "cover":
                s = max(sx, sy)
            elif fit == "none":
                s = 1.0
            elif fit == "scale-down":
                s = min(min(sx, sy), 1.0)
            else:                       # contain (and any unknown value)
                s = min(sx, sy)
            dw, dh = nw * s, nh * s
            ox, oy = self._object_position(bw - dw, bh - dh)
            cmds = []
            clip = dw > bw + 0.5 or dh > bh + 0.5   # cover/none overflow
            if clip:
                cmds.append(DrawClipPush(self.x, self.y,
                                         self.x + bw, self.y + bh))
            cmds.append(DrawImage(self.x + ox, self.y + oy, dw, dh, img[0]))
            if clip:
                cmds.append(DrawClipPop())
            return cmds
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
        sticky = _sticky_metrics(layout_object)
        if (tf and tf != "none") or sticky is not None:
            sub = _paint_tree_inner(layout_object, [])
            if tf and tf != "none":
                dx, dy, hidden = parse_transform(
                    tf, layout_object.width, layout_object.height)
                if hidden:
                    return display_list
                if dx or dy:
                    translate_cmds(sub, dx, dy)
            if sticky is not None:
                normal, maximum, inset = sticky
                display_list.append(DrawStickyPush(
                    normal, maximum, inset))
                display_list.extend(sub)
                display_list.append(DrawStickyPop())
            else:
                display_list.extend(sub)
            return display_list
    return _paint_tree_inner(layout_object, display_list)


def _paint_tree_inner(layout_object, display_list):
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
    # CSS painting order §E.2: negative-z-index children paint behind the
    # element's own background/border; everything else on top of it.
    for z, _i, sub in groups:
        if z >= 0:
            break
        display_list.extend(sub)
    display_list.extend(layout_object.paint())
    for z, _i, sub in groups:
        if z < 0:
            continue
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
