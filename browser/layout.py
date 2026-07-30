"""Layout engine: builds a box tree from the styled DOM.

DocumentLayout -> BlockLayout (block or inline mode)
              -> LineLayout -> TextLayout (one word each)
"""

import math
import re
import tkinter.font

from . import textengine
from .colors import NAMED, to_rgb
from .draw import (DrawBgImage, DrawClipPop, DrawClipPush, DrawGradient,
                   DrawImage, DrawLine, DrawOpacityPop, DrawOpacityPush,
                   DrawOval, DrawRect, DrawStickyPop,
                   DrawStickyPush, DrawText,
                   scale_cmds_about, translate_cmds)
from .html_parser import Element, Text, tree_to_list
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

# Form controls the engine paints itself (a "widget face"), whose
# children therefore never take part in layout: <select> renders the
# selected option's label + a chevron, <textarea> renders its current
# value. Their DOM children stay intact for submission, scripting and
# the accessibility tree — they simply do not flow.
REPLACED_CONTROLS = {"input", "select", "textarea"}

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


# Halfwidth forms and ASCII map onto the fullwidth block one for one;
# the ASCII range starts at U+FF01 and the space is its own codepoint.
_FULLWIDTH_OFFSET = 0xFF01 - ord("!")


def _fullwidth(word):
    out = []
    for ch in word:
        code = ord(ch)
        if ch == " ":
            out.append("\u3000")
        elif 0x21 <= code <= 0x7E:
            out.append(chr(code + _FULLWIDTH_OFFSET))
        else:
            out.append(ch)
    return "".join(out)


# CSS Text 3 §3: the collapsible white space is exactly space, tab and
# the segment breaks — line feed, plus the carriage return the parser
# turns into one. Everything else is a character with a glyph.
#
# Python disagrees on all of it. str.split(), str.isspace() and re's
# \s also swallow U+000B, U+000C, U+001C..U+001F, U+0085, U+00A0 and
# the whole Unicode space block, so a no-break space was collapsing
# exactly like an ordinary space and a form feed disappeared instead
# of rendering. Splitting text needs CSS's set, not Python's.
CSS_SPACE = " \t\n\r"
_CSS_GAP_RE = re.compile("[ \t\n\r]+")

# Collapsing is not the only thing "space" means. A second, wider set
# is what a line may break at and what hangs past the end of one:
# Unicode's space separators (Zs) plus tab, minus the ones whose whole
# purpose is not to break — U+00A0 no-break space, U+202F narrow
# no-break space and U+2007 figure space. U+3000 ideographic space is
# in it, which is why `ああ　ああ` wraps after the ideographic space
# even though nothing collapses it.
BREAK_SPACE = (
    " \t\u1680"                              # space, tab, ogham
    "\u2000\u2001\u2002\u2003\u2004\u2005\u2006"   # en quad .. six-per-em
    "\u2008\u2009\u200a\u205f\u3000")            # punctuation .. ideographic
_BRK_CLASS = "[" + "".join(
    "\\" + c if c in "\\]^-" else c for c in BREAK_SPACE) + "]"
_BRK_RUN_RE = re.compile(f"{_BRK_CLASS}+|(?:(?!{_BRK_CLASS}).)+")


def css_words(text):
    """`text` split on CSS white space, and on nothing else."""
    return [w for w in _CSS_GAP_RE.split(text) if w]


def break_runs(text):
    """`text` as alternating breaking-space and other runs."""
    return _BRK_RUN_RE.findall(text)


def leads_with_space(text):
    return bool(text) and text[0] in CSS_SPACE


def ends_with_space(text):
    return bool(text) and text[-1] in CSS_SPACE


# C0 and C1 controls that carry no white-space meaning. Tab, line feed
# and carriage return are excluded: white-space processing owns those.
_CONTROLS = frozenset(
    [chr(c) for c in range(0x00, 0x20) if c not in (0x09, 0x0A, 0x0D)]
    + [chr(c) for c in range(0x7F, 0xA0)])


def visible_controls(word):
    """Control characters rendered as something you can see.

    CSS Text 3 §white-space-processing requires a control character to
    be visible — a font draws most of them as nothing at all, so a
    document containing one would silently lose it. U+FFFD is the
    substitution every engine ends up making in some form.
    """
    if not word or not any(ch in _CONTROLS for ch in word):
        return word
    return "".join("\ufffd" if ch in _CONTROLS else ch for ch in word)


def transformed_text(node, word):
    """`text-transform` applied, with control characters made visible.
    It inherits, so the text node carries it — but nothing ever read
    it, so the property was inert."""
    word = visible_controls(word)
    how = (node.style.get("text-transform") or "none").strip().casefold()
    if how == "none":
        return word
    if how == "uppercase":
        return word.upper()
    if how == "lowercase":
        return word.lower()
    if how == "capitalize":
        # titlecase, not uppercase: the first letter of `ǆ` is `ǅ` and
        # of the `ﬁ` ligature is `Fi`. Only the first character is
        # touched — str.title() would also lowercase the rest, which
        # capitalize is not allowed to do.
        if not word:
            return word
        head = word[0].title()
        return head + word[1:]
    if how == "full-width":
        return _fullwidth(word)
    return word


def text_indent_px(style, width, font_size):
    """(indent in px, hanging) for a block's `text-indent`.

    `hanging` inverts which lines get it: every line except the first,
    which is how a dictionary entry or a bibliography is set.
    `each-line` re-indents after every forced break as well as at the
    start of the block. The two compose: `each-line hanging` indents
    every line that is not the first of a paragraph or of a <br>
    segment.
    """
    raw = str(style.get("text-indent", "0")).strip()
    if not raw or raw == "0":
        return 0.0, False, False
    tokens = raw.casefold().split()
    value = parse_size(tokens[0], width, font_size)
    if value is None:
        return 0.0, False, False
    return value, "hanging" in tokens, "each-line" in tokens


_INLINE_LEAD = ("margin-left", "border-left-width", "padding-left")
_INLINE_TRAIL = ("margin-right", "border-right-width", "padding-right")


def inline_insets(node, avail):
    """(leading, trailing) px an inline box opens around its content.

    Only the horizontal edges: an inline box's vertical margin and
    padding do not affect line height, which is why this is the half
    that matters and the half that was missing entirely — a padded
    <span> badge drew its text flush against whatever came before it.
    """
    style = getattr(node, "style", None) or {}
    if (style.get("display") or "inline").strip().casefold() != "inline":
        return 0.0, 0.0
    em = parse_px(style.get("font-size", "16px"), 16.0)

    def total(props):
        out = 0.0
        for prop in props:
            value = parse_size(style.get(prop), avail, em)
            if value:
                out += value
        return out

    return total(_INLINE_LEAD), total(_INLINE_TRAIL)


def word_spacing_px(node, font_size, space_advance=0.0):
    """`word-spacing` in px, added to each inter-word space.

    The initial `normal` adds nothing; a length (including a negative
    one, which tightens) is added to the space's own advance. A
    percentage is of the font size, not of the space — CSS Text 4 says
    so explicitly, and guessing the space's own advance as the basis
    gets a 100% value wrong by whatever the font's space happens to
    be."""
    raw = str(node.style.get("word-spacing", "normal")).strip()
    if not raw or raw.casefold() == "normal":
        return 0.0
    value = parse_size(raw, font_size, font_size)
    return value if value is not None else 0.0


def letter_spacing_px(node, font_size):
    """`letter-spacing` in px, 0 for the initial `normal`."""
    raw = str(node.style.get("letter-spacing", "normal")).strip()
    if not raw or raw == "normal":
        return 0.0
    try:
        if raw.endswith("em"):
            return float(raw[:-2]) * font_size
        if raw.endswith("px"):
            return float(raw[:-2])
        return float(raw)
    except ValueError:
        return 0.0


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


def _bg_size_token(token, base):
    """One background-size component as px, or None for auto."""
    t = (token or "auto").casefold()
    if t == "auto":
        return None
    if t.endswith("%"):
        try:
            return base * float(t[:-1]) / 100.0
        except ValueError:
            return None
    return parse_size(t)


def _vector_tile_size(spec, box_w, box_h, svg):
    """background-size for an image whose intrinsic size may be absent.

    CSS Images 3 §5.3, the default sizing algorithm: the background
    positioning area is the default object size, and an image missing
    a dimension falls back to the intrinsic ratio, then to that area.
    A file with no width, height or viewBox therefore *becomes* the
    box — `background-size: contain` on it fills the element rather
    than fitting a made-up 24x24 square into a corner of it.
    """
    def sane(w, h):
        # A viewBox of `0 0 2147483647 1` is a real file in the corpus,
        # and `cover` on it asks for a tile two billion pixels wide.
        # The rasterizer takes u32s, so the ask has to be bounded
        # before it gets there — the visible result is the same either
        # way, since only the part over the box is ever sampled.
        cap = 1e5
        if not (math.isfinite(w) and math.isfinite(h)) \
                or w > cap or h > cap:
            return (min(w, cap) if math.isfinite(w) else box_w,
                    min(h, cap) if math.isfinite(h) else box_h)
        return w, h

    if textengine.svg_degenerate(svg):
        return 0.0, 0.0                 # no visible content at any size
    iw, ih, ratio = textengine.svg_intrinsic(svg)
    size = spec["size"]
    kw = size[0].casefold() if size else "auto"
    if kw in ("cover", "contain"):
        if not ratio:
            return box_w, box_h
        h = (max if kw == "cover" else min)(box_h, box_w / ratio)
        return sane(h * ratio, h)
    w = _bg_size_token(size[0] if size else None, box_w)
    h = _bg_size_token(size[1] if len(size) > 1 else None, box_h)
    if w is not None and h is not None:
        return sane(w, h)
    if w is not None:
        return sane(w, (w / ratio if ratio else ih or box_h))
    if h is not None:
        return sane((h * ratio if ratio else iw or box_w), h)
    if iw and ih:                       # `auto auto`: the intrinsic size
        return sane(iw, ih)
    if ratio:                           # one dimension, or none: use it
        if iw:
            return sane(iw, iw / ratio)
        if ih:
            return sane(ih * ratio, ih)
        h = min(box_h, box_w / ratio)   # ratio constrained by the area
        return sane(h * ratio, h)
    return sane(iw or box_w, ih or box_h)


def paint_background_image(node, x1, y1, x2, y2):
    """DrawBgImage for node's first background layer, or None."""
    bg = getattr(node, "_bg", None)
    if not bg:
        return None
    image_id, iw, ih, spec = bg
    box_w, box_h = x2 - x1, y2 - y1
    if box_w <= 0 or box_h <= 0:
        return None
    vector = getattr(node, "_bg_vector", None)
    if vector is not None:
        tile_w, tile_h = _vector_tile_size(spec, box_w, box_h, vector)
        if tile_w < 1 or tile_h < 1:
            return None
        handle = textengine.rasterize_svg(vector, tile_w, tile_h)
        if handle is None:
            return None
        image_id = handle[0]
    else:
        if iw <= 0 or ih <= 0:
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


def _ellipsize(text, font, avail):
    """Longest prefix of `text` that fits `avail` px, with "…" when it
    had to cut. Used by the widget faces (a select label or a textarea
    line must never spill out of its control)."""
    text = text or ""
    if avail <= 0:
        return ""
    if measure(font, text) <= avail:
        return text
    ell = measure(font, "…")
    lo, hi = 0, len(text)
    while lo < hi:
        mid = (lo + hi + 1) // 2
        if measure(font, text[:mid]) + ell <= avail:
            lo = mid
        else:
            hi = mid - 1
    return text[:lo] + "…"


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


def paint_visible(node):
    """Whether a computed-style node contributes paint.

    `visibility` is inherited by the style engines, so checking the node's
    computed value also suppresses every descendant of a hidden subtree
    while preserving its layout geometry.
    """
    visibility = getattr(node, "style", {}).get(
        "visibility", "visible").strip().casefold()
    return visibility not in ("hidden", "collapse") \
        and effective_opacity(node) >= 0.05


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


def _color_alpha(tok):
    """The alpha a color token carries (1.0 when opaque/unknown)."""
    t = tok.strip().casefold()
    if t.startswith("#") and len(t) == 9:        # #rrggbbaa
        try:
            return int(t[7:9], 16) / 255.0
        except ValueError:
            return 1.0
    if t.startswith("#") and len(t) == 5:        # #rgba
        try:
            return int(t[4], 16) / 15.0
        except ValueError:
            return 1.0
    m = re.match(r"rgba?\([^)]*[,/]\s*(\d*\.?\d+)\s*\)$", t)
    if m and ("rgba(" in t or "/" in t):
        try:
            a = float(m.group(1))
            return a / 100.0 if t.rstrip(")").endswith("%") else a
        except ValueError:
            return 1.0
    return 1.0


_OUTLINE_STYLES = {"solid", "dashed", "dotted", "double", "groove",
                   "ridge", "inset", "outset", "auto"}


def outline_ring(style, em=16.0):
    """(width, color, offset) for an element's outline, or (0, "", 0).

    An outline is a ring drawn outside the border box that occupies no
    space, which is what makes it the focus indicator: it cannot shift
    the layout it is drawn around. `outline: none` and the initial
    `outline-style: none` both mean nothing is drawn, and the width is
    the keyword scale a UA picks for thin/medium/thick.
    """
    shorthand = (style.get("outline") or "").strip()
    width = style.get("outline-width")
    color = style.get("outline-color")
    line = (style.get("outline-style") or "").strip().casefold()
    if shorthand:
        for token in shorthand.split():
            low = token.casefold()
            if low in _OUTLINE_STYLES:
                line = line or low
            elif low in ("none", "hidden"):
                line = line or "none"
            elif width is None and (
                    low in ("thin", "medium", "thick")
                    or low[:1].isdigit() or low.startswith(".")):
                width = token
            elif color is None:
                color = token
    if not line or line in ("none", "hidden"):
        return 0.0, "", 0.0
    keyword = {"thin": 1.0, "medium": 3.0, "thick": 5.0}
    raw = (width or "medium").strip().casefold()
    px = keyword.get(raw)
    if px is None:
        px = parse_size(raw, 0.0, em)
    if px is None or px <= 0:
        return 0.0, "", 0.0
    offset = parse_size(style.get("outline-offset"), 0.0, em) or 0.0
    # `outline-color: invert` has no inverting rasterizer here; the
    # currentColor fallback is what the property's own initial value
    # resolves to in engines that dropped invert
    raw_color = (color or "").strip()
    if not raw_color or raw_color.casefold() == "invert":
        raw_color = style.get("color", "black")
    return px, safe_color(raw_color, default="black"), offset


def box_shadow(value):
    """Parse the first paintable box-shadow layer to (dx, dy, color), or
    None. Blur/spread are ignored (we paint a flat offset rect), so a
    NEAR-TRANSPARENT layer must not paint at all: we cannot alpha-blend,
    and naver's card outline `0 0 0 1px #0000001A, 0 1px 2px
    rgba(0,0,0,.04)` painted as an opaque black slab behind every card —
    visible as solid black boxes wherever the card had no background
    (empty ad slots). `inset` and `none` yield None."""
    layers, depth, start = [], 0, 0
    s = value or ""
    for i, ch in enumerate(s):
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth = max(depth - 1, 0)
        elif ch == "," and depth == 0:
            layers.append(s[start:i])
            start = i + 1
    layers.append(s[start:])
    for layer in layers:
        v = layer.strip()
        if not v or v == "none":
            return None
        if "inset" in v:
            continue
        color = ""
        alpha = 1.0
        nums = []
        for tok in v.split():
            c = safe_color(tok, default="")
            # a color-shaped token safe_color rejects (#rrggbbaa) still
            # decides the layer's alpha — otherwise the translucent
            # outline paints as the opaque fallback
            looks_color = tok.startswith("#") \
                or tok.casefold().startswith(("rgb", "hsl"))
            if (c or looks_color) and not color:
                color = c or "#000000"
                alpha = _color_alpha(tok)
            elif tok.endswith("px") or _num_re.fullmatch(tok):
                try:
                    nums.append(float(tok.rstrip("px")))
                except ValueError:
                    pass
        if len(nums) < 2:
            continue
        if alpha < 0.25:
            # a subtle tinted shadow: closer to invisible than to a
            # solid fill — skip this layer
            continue
        return nums[0], nums[1], color or "#000000"
    return None


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


def _split_commas(text):
    """Split on commas, ignoring those inside parentheses.
    rgba(0, 0, 0, .5) is one stop, not four."""
    out, depth, start = [], 0, 0
    for i, ch in enumerate(text):
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth = max(0, depth - 1)
        elif ch == "," and depth == 0:
            out.append(text[start:i])
            start = i + 1
    out.append(text[start:])
    return [p.strip() for p in out if p.strip()]


_ANGLE_UNITS = {"deg": 1.0, "grad": 0.9, "rad": 180.0 / math.pi,
                "turn": 360.0}


def _angle_deg(token):
    """A CSS <angle> in degrees, or None if the token is not one."""
    t = token.strip().casefold()
    for unit, scale in _ANGLE_UNITS.items():
        if t.endswith(unit):
            try:
                return float(t[:-len(unit)]) * scale
            except ValueError:
                return None
    return None


def _gradient_direction(head, w, h):
    """The gradient's angle in degrees from its first argument.

    `to <corner>` is not a fixed angle: the line has to be tilted so
    that the corner-to-corner edge stays perpendicular to it, which
    depends on the box's own proportions. Without that, a
    `to bottom right` gradient in a wide box points visibly wrong.
    """
    if head is None:
        return 180.0                              # to bottom
    angle = _angle_deg(head)
    if angle is not None:
        return angle
    words = head.casefold().split()
    if not words or words[0] != "to":
        return None
    sides = set(words[1:])
    if not sides or not sides <= {"top", "bottom", "left", "right"}:
        return None
    if len(sides) == 1:
        return {"top": 0.0, "right": 90.0,
                "bottom": 180.0, "left": 270.0}[sides.pop()]
    if w <= 0 or h <= 0:
        return 180.0
    corner = math.degrees(math.atan2(w, h))
    if sides == {"top", "right"}:
        return corner
    if sides == {"bottom", "right"}:
        return 180.0 - corner
    if sides == {"bottom", "left"}:
        return 180.0 + corner
    if sides == {"top", "left"}:
        return 360.0 - corner
    return None


def _stop_offset(token, line_len):
    """A stop position as a 0..1 fraction of the gradient line."""
    t = token.strip().casefold()
    if t.endswith("%"):
        try:
            return float(t[:-1]) / 100.0
        except ValueError:
            return None
    px = parse_size(t)
    if px is None or line_len <= 0:
        return None
    return px / line_len


def parse_linear_gradient(value, w, h):
    """(angle_deg, [(offset, (r, g, b), alpha)]) for a linear-gradient.

    None for anything else — radial and conic sweeps still fall back to
    the solid approximation, and so does a gradient whose colours this
    engine cannot parse.
    """
    if not value:
        return None
    text = value.strip()
    low = text.casefold()
    start = low.find("linear-gradient(")
    if start < 0:
        return None
    # repeating-linear-gradient repeats the stop list past its end;
    # drawing one period of it is wrong, so leave it to the fallback
    if low[:start].endswith("repeating-"):
        return None
    depth, end = 0, -1
    for i in range(start + len("linear-gradient"), len(text)):
        if text[i] == "(":
            depth += 1
        elif text[i] == ")":
            depth -= 1
            if depth == 0:
                end = i
                break
    if end < 0:
        return None
    args = _split_commas(text[start + len("linear-gradient("):end])
    if not args:
        return None
    head = args[0]
    angle = _gradient_direction(
        head if (_angle_deg(head) is not None
                 or head.casefold().startswith("to ")) else None, w, h)
    if angle is None:
        return None
    if _angle_deg(head) is not None or head.casefold().startswith("to "):
        args = args[1:]
    if len(args) < 2:
        return None
    rad = math.radians(angle)
    line_len = abs(w * math.sin(rad)) + abs(h * math.cos(rad))

    stops = []
    for arg in args:
        parts = arg.split()
        color = safe_color(parts[0], default="")
        if not color:
            # a bare position with no colour is an interpolation hint;
            # anything else is a colour this engine does not know, and
            # guessing at it would paint the box a colour nobody asked for
            if len(parts) == 1 and _stop_offset(parts[0], line_len) \
                    is not None:
                continue
            return None
        alpha = _color_alpha(parts[0])
        # `<color> <p1> <p2>` is shorthand for two stops at both ends
        for pos in (parts[1:] or [None]):
            stops.append([_stop_offset(pos, line_len) if pos else None,
                          to_rgb(color), alpha])
    if len(stops) < 2:
        return None

    # positions default per CSS Images 3 s3.4.3: ends anchor, unpositioned
    # runs spread evenly between their positioned neighbours, and the list
    # is forced non-decreasing so a smaller position never runs backwards
    if stops[0][0] is None:
        stops[0][0] = 0.0
    if stops[-1][0] is None:
        stops[-1][0] = 1.0
    i = 0
    while i < len(stops):
        if stops[i][0] is not None:
            stops[i][0] = max(stops[i][0], stops[i - 1][0] if i else 0.0)
            i += 1
            continue
        run = i
        while stops[run][0] is None:
            run += 1
        lo, hi = stops[i - 1][0], stops[run][0]
        for k in range(i, run):
            stops[k][0] = lo + (hi - lo) * (k - i + 1) / (run - i + 1)
        i = run
    return angle, [(o, rgb, a) for o, rgb, a in stops]


def _child_is_block_level(child):
    """Whether a child forces its container into block flow. Computed
    display overrides the tag default, so `<li style="display:inline-block">`
    is inline-level (an atomic box on a line), not a block — otherwise a
    grid of inline-block cards would stack vertically."""
    if not isinstance(child, Element):
        return False
    # An out-of-flow child is not in the flow it would have to break:
    # `<div style="line-clamp:2"><div style="position:absolute"></div>
    # text</div>` is still a block with only inline content, and
    # wrapping the text in an anonymous block instead loses the clamp.
    if child.style.get("position", "static").strip().casefold() in (
            "absolute", "fixed"):
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


# Inherited text properties an anonymous inline box carries over from
# its block parent (fonts/line metrics for the words it will lay out).
_ANON_INHERITED = (
    "font-size", "font-style", "font-weight", "font-family", "color",
    "text-align", "white-space", "line-height", "letter-spacing",
    "word-spacing", "word-break", "overflow-wrap", "direction",
)


def _effective_block_children(node):
    """CSS 9.2.1.1 anonymous block boxes, narrowly: in a block context,
    CONSECUTIVE inline-level siblings that include real text are wrapped
    in one anonymous inline container so they share wrapped lines.
    Without this, `<div>27.1 ° <div>맑음</div></div>` stacked "27.1",
    "°" and the status each on its own row (naver's weather card).

    Kept conservative: runs must contain a non-whitespace Text node;
    lone inline ELEMENTS keep the legacy own-block path (they may carry
    padding/borders that only the block painter renders), and
    inline-block children stay in the row-cursor path."""
    out = []
    run = []

    def run_qualifies(child):
        if isinstance(child, Text):
            return True
        if not isinstance(child, Element):
            return False
        if _child_is_block_level(child) or is_out_of_flow(child):
            return False
        # inline-blocks may join a TEXT-bearing run (a logo <i> beside
        # a label shares its line); runs without text fall back to the
        # row-cursor path, so card grids are unaffected
        return True

    def flush():
        if not run:
            return
        live = [c for c in run
                if not (isinstance(c, Text) and not c.text.strip())]
        has_text = any(isinstance(c, Text) for c in live)
        if len(live) >= 2 and has_text:
            anon = Element("gg-anon", {}, node)
            anon.style = {p: v for p in _ANON_INHERITED
                          if (v := node.style.get(p))}
            anon.children = list(run)
            anon._font = None
            out.append(anon)
        else:
            out.extend(run)
        run.clear()

    for child in node.children:
        if isinstance(child, Element) and not is_visible(child):
            continue  # no box: does not break the run
        if run_qualifies(child):
            if isinstance(child, Text) and not child.text.strip() \
                    and not run:
                out.append(child)  # leading formatting whitespace
                continue
            run.append(child)
        else:
            flush()
            out.append(child)
    flush()
    return out


class _FlowMarker:
    """Stand-in 'previous sibling' marking the bottom of an inline-block
    row, so the next in-flow block clears the row. Never painted — only its
    y/height feed the next box's vertical placement."""
    __slots__ = ("y", "height", "pb", "bw", "bb", "margin_bottom")

    def __init__(self, y, height):
        self.y = y
        self.height = height
        self.pb = 0
        self.bw = 0
        self.bb = 0
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
    # beside it, not push the text into a stacked block row. An anonymous
    # inline run is by construction inline (its inline-blocks share the
    # line with the run's text — a logo <i> beside its label).
    if node.tag != "gg-anon" and any(
            isinstance(child, Element)
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


_WS_KEYWORDS = ("normal", "pre", "nowrap", "pre-wrap", "pre-line",
                "break-spaces")
# (white-space-collapse, wraps) -> the white-space keyword that means it
_WS_FROM_LONGHANDS = {
    ("collapse", True): "normal",
    ("collapse", False): "nowrap",
    ("preserve", True): "pre-wrap",
    ("preserve", False): "pre",
    ("preserve-breaks", True): "pre-line",
    ("preserve-breaks", False): "pre",
    ("break-spaces", True): "break-spaces",
    ("break-spaces", False): "pre",
}


def white_space(style):
    """The effective white-space keyword for a style dict.

    CSS Text 4 turned white-space into a shorthand over
    white-space-collapse and text-wrap-mode (spelled text-wrap in the
    older drafts, and both spellings are still in the corpus). Layout
    only ever wants the one answer, so the longhands are folded back
    into the keyword they are equivalent to, and a longhand set
    directly wins over the shorthand — which is what the cascade would
    do anyway if the shorthand expanded properly.
    """
    ws = (style.get("white-space") or "normal").strip().casefold()
    if ws not in _WS_KEYWORDS:
        ws = "normal"
    collapse = (style.get("white-space-collapse") or "").strip().casefold()
    wrap = (style.get("text-wrap-mode")
            or style.get("text-wrap") or "").strip().casefold()
    if not collapse and not wrap:
        return ws
    if not collapse:
        collapse = {"pre": "preserve", "pre-wrap": "preserve",
                    "break-spaces": "break-spaces",
                    "pre-line": "preserve-breaks"}.get(ws, "collapse")
    # text-wrap also carries balance/pretty/stable, which are wrapping
    # *styles*: they all still wrap
    wraps = wrap != "nowrap" if wrap else ws not in ("pre", "nowrap")
    return _WS_FROM_LONGHANDS.get((collapse, wraps), ws)


_PHYSICAL_ALIGN = {"left", "right", "center", "justify"}
_JUSTIFY_ALL = "justify-all"


def resolved_text_align(node, is_last_line=False):
    """The physical alignment of one line: left, right, center, justify.

    `start`/`end` are the writing-mode-relative spellings and are the
    initial value's real name, so a block that sets neither is `start`.
    `match-parent` resolves against the parent's own value, which is the
    only way to say "inherit, but resolve start/end in the parent's
    direction". And the final line of a block takes text-align-last
    instead, which is what makes `justify` leave its last line ragged
    rather than stretching three words across the column.
    """
    style = getattr(node, "style", None) or {}
    align = (style.get("text-align") or "start").strip().casefold()
    if align == "justify-all":
        # justify-all is justify that also stretches the last line, so
        # it is the one value text-align-last cannot override
        return "justify"
    if is_last_line:
        last = (style.get("text-align-last") or "").strip().casefold()
        if last and last != "auto":
            align = last
        elif align == "justify":
            align = "start"
    rtl = (style.get("direction") or "ltr").strip().casefold() == "rtl"
    for _ in range(8):
        if align in _PHYSICAL_ALIGN:
            return align
        if align in ("start", "normal", ""):
            return "right" if rtl else "left"
        if align == "end":
            return "left" if rtl else "right"
        if align != "match-parent":
            return "left"
        parent = getattr(node, "parent", None)
        pstyle = getattr(parent, "style", None) or {}
        align = (pstyle.get("text-align") or "start").strip().casefold()
        rtl = (pstyle.get("direction") or "ltr").strip().casefold() == "rtl"
        node = parent
        if node is None:
            return "right" if rtl else "left"
    return "left"


def lh_unit(node, em):
    """The px the `lh` unit measures: the element's used line-height,
    falling back to the font's own line spacing when line-height is
    `normal` (which is what `normal` means)."""
    target = line_height_px(node, em)
    if target is not None:
        return target
    try:
        return cached_font(node).gg_linespace
    except Exception:
        return em * 1.2


def clamp_lines(box, n, ellipsis):
    """Keep the first `n` line boxes of a laid-out subtree and mark the
    cut with `ellipsis`.

    `-webkit-box` counts lines across the block children it stacks, not
    only the ones in its own inline context, so this cut has to be made
    on the finished tree — during the inline walk those lines belong to
    a descendant that has not been laid out yet.
    """
    kept = [0]

    def walk(b):
        out = []
        for child in b.children:
            if isinstance(child, LineLayout):
                if kept[0] >= n:
                    continue
                kept[0] += 1
                out.append(child)
                if kept[0] == n and ellipsis:
                    words = [w for w in child.children
                             if isinstance(w, TextLayout)]
                    if words:
                        words[-1].word = words[-1].word.rstrip() + ellipsis
            else:
                walk(child)
                out.append(child)
        b.children = out

    walk(box)
    _reheight(box)
    return kept[0]


def _reheight(box):
    """Recompute a trimmed subtree's heights bottom-up: a block of line
    boxes is as tall as they are, anything else reaches its last
    child's bottom edge."""
    for child in box.children:
        if isinstance(child, BlockLayout):
            _reheight(child)
    if not box.children:
        box.height = 0.0
        return
    if all(isinstance(c, LineLayout) for c in box.children):
        box.height = sum(c.height for c in box.children)
    else:
        last = box.children[-1]
        box.height = max(
            (last.y + last.height + getattr(last, "pb", 0)
             + getattr(last, "bb", 0)) - box.y, 0.0)


def line_clamp_auto(style):
    """`line-clamp: auto` — clamp to whatever the box's own height
    allows rather than to a line count (CSS Overflow 4)."""
    return (style.get("line-clamp") or "").strip().casefold() == "auto"


def line_clamp_count(style):
    """How many lines this element clamps to, or None.

    The two spellings do not have the same trigger. `-webkit-line-clamp`
    is the legacy property and only applies inside the legacy box
    (`display: -webkit-box` with `-webkit-box-orient: vertical`) — an
    element that merely names it is not clamped, which is exactly what
    the corpus tests. `line-clamp` is the CSS Overflow 4 property and
    applies to any block container.
    """
    modern = (style.get("line-clamp") or "").strip().casefold()
    legacy = (style.get("-webkit-line-clamp") or "").strip().casefold()
    value = modern
    if not value or value == "none":
        display = (style.get("display") or "").strip().casefold()
        orient = (style.get("-webkit-box-orient")
                  or style.get("box-orient") or "").strip().casefold()
        if display not in ("-webkit-box", "-webkit-inline-box") \
                or orient != "vertical":
            return None
        value = legacy
    if not value.isdigit():
        return None
    return int(value) or None


def block_ellipsis(style):
    """The string that marks a clamped line. `block-ellipsis` can name
    its own, and `none` means the line just ends."""
    spec = (style.get("block-ellipsis") or "").strip()
    if not spec or spec.casefold() == "auto":
        return "…"
    if spec.casefold() == "none":
        return ""
    if len(spec) >= 2 and spec[0] == spec[-1] and spec[0] in "\"'":
        return spec[1:-1]
    return spec


def _ch_width(node):
    """The advance of "0" in a node's own font, which is what `ch`
    means. None when there is no measurable font to ask."""
    try:
        return measure(cached_font(node), "0") or None
    except Exception:
        return None


def _static_offset(mode, available, used):
    """How far an item is pushed inside its alignment container by an
    alignment keyword. `stretch`/`normal` do not move a box that has a
    size of its own, which an out-of-flow box always does.

    An item bigger than the container has negative free space, and the
    keyword still applies to it — `end` on an overflowing box hangs it
    off the start edge. That is what the `safe` overflow-position
    exists to prevent: with `safe`, an overflowing item falls back to
    start so its beginning stays reachable.
    """
    tokens = (mode or "").strip().casefold().split()
    if not tokens:
        return 0.0
    safe = "safe" in tokens
    keyword = tokens[-1]
    free = available - used
    if free < 0 and safe:
        return 0.0
    if keyword == "center":
        return free / 2
    if keyword in ("end", "flex-end", "right", "self-end"):
        return free
    return 0.0


def distribute_free_space(free, count, mode):
    """(leading offset, extra gap) for a content-distribution keyword.

    This is the CSS Box Alignment §5 job of `justify-content` and
    `align-content` on a grid container: the tracks are already sized,
    and whatever is left over is placed around them. `normal`/`stretch`
    leave it alone — track sizing has already had its chance to absorb
    it, and anything it did not take stays at the end.
    """
    if free <= 0 or count < 1:
        return 0.0, 0.0
    mode = (mode or "").strip().casefold().split()[-1:]
    mode = mode[0] if mode else ""
    if mode in ("end", "flex-end", "right"):
        return free, 0.0
    if mode == "center":
        return free / 2, 0.0
    if mode == "space-between":
        return (0.0, free / (count - 1)) if count > 1 else (0.0, 0.0)
    if mode == "space-around":
        gap = free / count
        return gap / 2, gap
    if mode == "space-evenly":
        gap = free / (count + 1)
        return gap, gap
    return 0.0, 0.0


def gap_rule(style, axis, em):
    """(width, colour) of a gap decoration rule, or None.

    CSS Gap Decorations paints a rule down the middle of each gap of a
    grid or flex container: `column-rule-*` in the column gaps,
    `row-rule-*` in the row gaps. Longhands win over the shorthand,
    and a rule with no style, no width or no colour draws nothing —
    the same three-part test a border passes.
    """
    prefix = f"{axis}-rule"
    width = style.get(f"{prefix}-width")
    line = style.get(f"{prefix}-style")
    color = style.get(f"{prefix}-color")
    short = (style.get(prefix) or "").split()
    for token in short:
        t = token.strip().casefold()
        if t in _BORDER_STYLES:
            line = line or t
        elif safe_color(t, default=""):
            color = color or t
        else:
            width = width or t
    if (line or "none").strip().casefold() in ("none", "hidden"):
        return None
    px = parse_size(width, 0, em) if width else 3.0
    if px is None or px <= 0:
        return None
    rgb = safe_color(color, default="") if color else ""
    if not rgb:
        rgb = safe_color(style.get("color"), default="") or "#000000"
    return px, rgb


_BORDER_STYLES = frozenset((
    "none", "hidden", "dotted", "dashed", "solid", "double",
    "groove", "ridge", "inset", "outset"))


def auto_size(style, prop):
    """Is this box's size in one axis auto?

    Stretch alignment only reaches a box whose size in that axis is
    auto (CSS Box Alignment 3 §4.2) — a specified width or height is
    the used size and alignment then positions the box inside its area
    instead of growing it.
    """
    raw = (style.get(prop) or "").strip().casefold()
    return not raw or raw == "auto"


def order_items(children):
    """Flex and grid items in `order` sequence.

    `order` reorders the boxes without touching the document: paint
    order, tab order and the accessibility tree all still follow source
    order, which is exactly why the sort has to be stable. An item that
    does not set it sits at 0, so a single `order: -1` moves one item
    to the front without renumbering the rest — which is the whole way
    the property gets used.
    """
    if not any((getattr(c, "style", None) or {}).get("order")
               for c in children):
        return children

    def key(child):
        raw = ((getattr(child, "style", None) or {}).get("order") or "")
        try:
            return int(float(raw.strip()))
        except ValueError:
            return 0

    return sorted(children, key=key)


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


def _style_border_width(style, side, avail, em):
    """Used border width for one physical side, with shorthand fallback."""
    raw = style.get(f"border-{side}-width")
    if raw is None:
        raw = style.get("border-width")
    return parse_size(raw, avail, em) or 0.0


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


def _inherited_kw(node, prop):
    """A keyword this engine does not inherit, read off the nearest
    ancestor that sets one. word-break, overflow-wrap and line-break
    all inherit in CSS and none of them are in the inherited table, so
    reading the node's own style alone finds nothing."""
    n = node
    while n is not None:
        v = n.style.get(prop) if hasattr(n, "style") else None
        if v:
            return v.strip().casefold()
        n = getattr(n, "parent", None)
    return ""


def hang_trim(word):
    """`word` without its trailing breaking spaces.

    Those hang past the end of the line (CSS Text 3 §5.2), so they are
    not part of what has to fit: `X<U+3000>` needs room for the X.
    """
    i = len(word)
    while i and word[i - 1] in BREAK_SPACE:
        i -= 1
    return word[:i]


# UAX 14, the part that matters once a line may break between any two
# ideographs: some characters refuse to be left dangling. Nothing
# breaks after an opening bracket or quote (class OP), and nothing
# breaks before a closing one, a full stop, a comma, a small kana or a
# sound mark (classes CL, CP, EX, IS, NS). Without this `中中‚文` in a
# box three ideographs wide wraps as `中中‚` / `文`, stranding the
# opening quote at the end of a line where no typesetter would leave it.
_LB_NO_BREAK_AFTER = frozenset(
    "([{‘“‚„〈《「『【〔"
    "〖〘〚〝（［｛｟｢⦅"
    "$£¥€￡￥＄＃")
_LB_NO_BREAK_BEFORE = frozenset(
    ")]}’”〉》」』】〕〗〙"
    "〛〞）］｝｠｣⦆"
    "!?！？‼⁇⁈⁉"
    ",.:;、。，．：；"
    "ー〜ゝゞヽヾ々‐–—・"
    "ぁぃぅぇぉっゃゅょゎ"
    "ァィゥェォッャュョヮ"
    "ヵヶ"
    "%‰°′″℃¢")


def break_segments(word, cjk=True):
    """`word` cut at its break opportunities.

    Two kinds, and they are not the same rule. A breaking space always
    allows a break after itself, and stays on the line it ends. An
    ideograph, kana or Hangul syllable may both start a line and end
    one — that is the opportunity `word-break: keep-all` suppresses,
    which is why U+3000 keeps breaking under keep-all: despite the
    name, ideographic space is a space, not an ideograph.
    """
    segs, buf = [], ""
    for i, ch in enumerate(word):
        if ch in BREAK_SPACE:
            segs.append(buf + ch)
            buf = ""
            continue
        opens = cjk and (_is_cjk(ch) or (buf and _is_cjk(buf[-1])))
        if opens and buf and buf[-1] not in _LB_NO_BREAK_AFTER \
                and ch not in _LB_NO_BREAK_BEFORE:
            segs.append(buf)
            buf = ""
        buf += ch
    if buf:
        segs.append(buf)
    return segs


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


_BULLETS = ("disc", "circle", "square")
_ROMAN = ((1000, "m"), (900, "cm"), (500, "d"), (400, "cd"), (100, "c"),
          (90, "xc"), (50, "l"), (40, "xl"), (10, "x"), (9, "ix"),
          (5, "v"), (4, "iv"), (1, "i"))


def list_style_type(node):
    """The marker style in effect for an <li>.

    It is an inherited property, so the <ul>/<ol> that sets it is
    usually not the element being painted; and when nothing sets it,
    the list's own tag decides — <ol> counts, <ul> bullets. That
    default is why every ordered list on every page was drawing dots.
    """
    n = node
    for _ in range(12):
        if not isinstance(n, Element):
            break
        for prop in ("list-style-type", "list-style"):
            raw = (n.style.get(prop) or "").strip().casefold()
            if not raw:
                continue
            for token in raw.split():
                if token in ("none", "inside", "outside") \
                        or token.startswith("url("):
                    continue
                return token
            if "none" in raw.split():
                return "none"
        if n.tag in ("ol", "ul"):
            return "decimal" if n.tag == "ol" else "disc"
        n = getattr(n, "parent", None)
    return "disc"


def _int_attr(el, name, default):
    try:
        return int(str(el.attributes.get(name, "")).strip())
    except (TypeError, ValueError):
        return default


def _list_item_index(node):
    """The counter value an <li> shows.

    Counting starts at the list's `start` attribute (1 by default, and
    the count runs backwards from the item total when the list is
    `reversed`), and any item's own `value` attribute resets it. All
    three are ordinary HTML that a numbered list uses the moment it is
    split across two <ol>s or counts down.
    """
    parent = getattr(node, "parent", None)
    if not isinstance(parent, Element):
        return 1
    items = [c for c in parent.children
             if isinstance(c, Element) and c.tag == "li"]
    reverse = parent.attributes.get("reversed") is not None
    index = _int_attr(parent, "start",
                      len(items) if reverse else 1)
    step = -1 if reverse else 1
    for child in items:
        index = _int_attr(child, "value", index)
        if child is node:
            return index
        index += step
    return index


def _alpha_label(n, upper=False):
    """1 -> a, 26 -> z, 27 -> aa. Bijective base 26, not base 26 with a
    zero digit — there is no 'a0' in an alphabetic list."""
    if n < 1:
        return str(n)
    out = ""
    while n > 0:
        n, rem = divmod(n - 1, 26)
        out = chr(ord("a") + rem) + out
    return out.upper() if upper else out


def _roman_label(n, upper=False):
    if not 0 < n < 4000:
        return str(n)
    out = ""
    for value, sym in _ROMAN:
        while n >= value:
            out += sym
            n -= value
    return out.upper() if upper else out


def marker_label(kind, index):
    """The text a counting marker draws, trailing separator included."""
    if kind in ("decimal", "decimal-leading-zero"):
        text = f"{index:02d}" if kind.endswith("zero") and 0 <= index < 10 \
            else str(index)
    elif kind in ("lower-alpha", "lower-latin"):
        text = _alpha_label(index)
    elif kind in ("upper-alpha", "upper-latin"):
        text = _alpha_label(index, upper=True)
    elif kind == "lower-roman":
        text = _roman_label(index)
    elif kind == "upper-roman":
        text = _roman_label(index, upper=True)
    else:
        # An unknown name is a @counter-style this engine does not
        # define, and the spec's fallback for one is decimal — not
        # "draw nothing", which is what returning "" here would do to
        # every list whose style came from an at-rule.
        text = str(index)
    return text + "."


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
    """One grid track, as one of

        ("fixed", px)              a resolved length or percentage
        ("fr", n)                  flexible, takes a share of free space
        ("intrinsic", lo, hi)      sized from the content it holds

    where lo/hi are "min", "max", or a px cap. auto, min-content,
    max-content, fit-content() and minmax() all land in the third form:
    they are the tracks whose size is a question about their items, and
    treating them as 1fr (which this did) gives a column its share of
    the container instead of the width of what is in it — `auto 1fr`,
    the ordinary sidebar-and-content shape, came out as a 50/50 split.
    """
    t = token.strip().casefold()
    if t.endswith("fr"):
        try:
            return ("fr", max(float(t[:-2]), 0.0))
        except ValueError:
            return ("fr", 1.0)
    if t.startswith("minmax(") and t.endswith(")"):
        parts = _split_commas(t[7:-1])
        if len(parts) == 2:
            lo = _grid_track_size(parts[0], avail, em)
            hi = _grid_track_size(parts[1], avail, em)
            # a flexible minimum is not a thing; a flexible maximum is
            # the track growing into free space after its floor is met
            if hi[0] == "fr":
                return ("fr", hi[1]) if lo[0] == "fr" else \
                    ("frmin", hi[1], _track_bound(lo, "min"))
            return ("intrinsic", _track_bound(lo, "min"),
                    _track_bound(hi, "max"))
    if t.startswith("fit-content(") and t.endswith(")"):
        cap = parse_size(t[12:-1], avail, em)
        return ("intrinsic", "min", cap if cap is not None else "max")
    if t == "auto":
        # "auto" is max-content as a growth limit, but unlike
        # max-content it also stretches into leftover space
        return ("intrinsic", "min", "auto")
    if t in ("min-content", "fit-content"):
        return ("intrinsic", "min", "min")
    if t == "max-content":
        return ("intrinsic", "max", "max")
    px = parse_size(token, avail, em)
    if px is not None:
        return ("fixed", max(px, 0.0))
    return ("intrinsic", "min", "max")


def _track_bound(track, side):
    """One end of a minmax() as a bound the sizer understands."""
    if track[0] == "fixed":
        return track[1]
    if track[0] == "intrinsic":
        return track[1] if side == "min" else track[2]
    return side


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


def _parse_grid_template(spec, avail, em, gap=0.0):
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
                sub = _split_top_level(inner[comma + 1:].strip())
                try:
                    count = int(inner[:comma].strip())
                except ValueError:
                    # auto-fill/auto-fit: as many copies of the pattern as
                    # fit the container. Repeating it once (which this did)
                    # turned every responsive card grid into one column.
                    count = _auto_repeat_count(sub, avail, gap, em)
                count = max(1, min(count, 1000))
                for _ in range(count):
                    add_parts(sub)
                continue
            tracks.append(_grid_track_size(p, avail, em))

    add_parts(_split_top_level(spec))
    return tracks, line_names


def _parse_grid_tracks(spec, avail, em, gap=0.0):
    """Compatibility helper for callers interested only in track sizes."""
    return _parse_grid_template(spec, avail, em, gap)[0]


def _auto_repeat_count(sub, avail, gap, em):
    """How many copies of a repeat(auto-fill, ...) pattern fit.

    Only a definitely-sized pattern can be counted; an intrinsic track
    has no width until its items are known, and the spec says such a
    repeat resolves to a single copy.
    """
    sizes = []
    for part in sub:
        p = part.strip()
        if p.startswith("[") and p.endswith("]"):
            continue
        track = _grid_track_size(p, avail, em)
        if track[0] == "fixed":
            sizes.append(track[1])
        elif track[0] == "frmin" and not isinstance(track[2], str):
            sizes.append(float(track[2]))
        else:
            return 1
    span = sum(sizes) + gap * len(sizes)
    if span <= 0 or avail <= 0:
        return 1
    return max(1, int((avail + gap) // span))


def _size_grid_tracks(tracks, avail, gap, contributions,
                      stretch=True):
    """Resolve a track list to pixel sizes (CSS Grid 1 s12, abridged).

    `contributions[i]` is (min_content, max_content) for the items whose
    span ends in track i — a spanning item's demand is spread over the
    tracks it covers before this is called, which is where the real
    algorithm is far more careful about which track absorbs it.

    The order matters and is the spec's: intrinsic tracks reach their
    base size first, free space then grows them toward their limit, and
    only what is left after that goes to the fr tracks. Doing it the
    other way round gives every fr track the whole container and leaves
    the content-sized ones at zero.
    """
    n = len(tracks)
    if not n:
        return []
    base = [0.0] * n
    limit = [float("inf")] * n

    def bound(spec, lo, hi):
        if spec == "min":
            return lo
        if spec in ("max", "auto"):
            return hi
        return float(spec)

    for i, track in enumerate(tracks):
        lo, hi = contributions[i] if i < len(contributions) else (0.0, 0.0)
        kind = track[0]
        if kind == "fixed":
            base[i] = limit[i] = track[1]
        elif kind == "fr":
            base[i] = 0.0
        elif kind == "frmin":
            base[i] = bound(track[2], lo, hi)
        else:                                    # intrinsic
            base[i] = bound(track[1], lo, hi)
            limit[i] = max(bound(track[2], lo, hi), base[i])

    free = avail - gap * max(n - 1, 0) - sum(base)

    # grow intrinsic tracks toward their growth limit, evenly, until
    # either the limits or the free space run out
    growable = [i for i, t in enumerate(tracks)
                if t[0] == "intrinsic" and limit[i] > base[i]]
    while free > 1e-6 and growable:
        share = free / len(growable)
        stalled = []
        for i in growable:
            room = limit[i] - base[i]
            take = min(share, room)
            base[i] += take
            free -= take
            if limit[i] - base[i] > 1e-6:
                stalled.append(i)
        if len(stalled) == len(growable):
            break                                # nothing hit its limit
        growable = stalled

    flex = [i for i, t in enumerate(tracks) if t[0] in ("fr", "frmin")]
    if flex and free > 0:
        total = sum(tracks[i][1] for i in flex)
        if total > 0:
            for i in flex:
                base[i] += free * tracks[i][1] / total
        return base

    # CSS Grid 1 s12.8: with no flexible track to soak it up, leftover
    # space is shared equally by the auto-max tracks -- which is why
    # `grid-template-columns: auto auto` fills its container instead of
    # hugging two words. max-content tracks are explicitly not stretched.
    if stretch and free > 1e-6:
        auto = [i for i, t in enumerate(tracks)
                if t[0] == "intrinsic" and t[2] == "auto"]
        if auto:
            for i in auto:
                base[i] += free / len(auto)
    return base


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
    normal = box.y - box.pt - box.bt - box.margin_top
    parent_bottom = (box.parent.y + box.parent.height
                     + getattr(box.parent, "pb", 0.0))
    maximum = max(normal, parent_bottom - box.outer_height())
    return normal, maximum, inset


# --- overflow scroll containers ------------------------------------
#
# A box with `overflow: auto|scroll` and a definite height becomes its
# own scroller: it clips its descendants and offsets them by a scroll
# position kept on the NODE (so it survives relayout, exactly like a
# frame's own scroll in browser/frames.py). The offset is applied at
# paint time — inner scrolling is driven by wheel events, which the
# shells already process, so it costs a repaint rather than the
# resident-display-list machinery the page scroll needs.

SCROLLABLE_OVERFLOW = ("auto", "scroll")


def _definite_size(node, prop):
    """A specified, non-auto length for the given axis, if any."""
    for name in (prop, "max-" + prop):
        raw = (node.style.get(name) or "").strip().casefold()
        if raw and raw not in ("auto", "none", "inherit", "initial",
                               "unset", "fit-content", "max-content",
                               "min-content"):
            return True
    return False


def scroll_axes(node):
    """(scrolls_y, scrolls_x) for a node's own box.

    Only a box that is BOTH scrollable and definitely sized can
    actually overflow: an `overflow:auto` box with content-driven
    height simply grows, and clipping one would hide readable content
    (which is why `auto` was historically left unclipped)."""
    if not isinstance(node, Element):
        return (False, False)
    style = node.style
    generic = (style.get("overflow") or "").strip().casefold()
    oy = (style.get("overflow-y") or "").strip().casefold() or generic
    ox = (style.get("overflow-x") or "").strip().casefold() or generic
    return (oy in SCROLLABLE_OVERFLOW and _definite_size(node, "height"),
            ox in SCROLLABLE_OVERFLOW and _definite_size(node, "width"))


def is_scroll_container(obj):
    node = getattr(obj, "node", None)
    return any(scroll_axes(node)) if node is not None else False


def _subtree_extent(obj):
    """(right, bottom) of the deepest painted descendant geometry.

    Memoized on the layout object: hit-testing asks every box for its
    ancestors' scroll offsets, and re-walking a scroller's subtree each
    time would make a hit test quadratic in page size. Layout objects
    are rebuilt from scratch by every relayout, so the cache can never
    go stale."""
    cached = getattr(obj, "_extent_cache", None)
    if cached is not None:
        return cached
    right = bottom = 0.0
    stack = list(getattr(obj, "children", ()) or ())
    while stack:
        box = stack.pop()
        w = getattr(box, "width", None)
        h = getattr(box, "height", None)
        if w is not None:
            right = max(right, box.x + w)
        if h is not None:
            bottom = max(bottom, box.y + h)
        stack.extend(getattr(box, "children", ()) or ())
    try:
        obj._extent_cache = (right, bottom)
    except AttributeError:
        pass  # __slots__ layout object: recompute each time
    return right, bottom


def scroll_range(obj):
    """(max_scroll_y, max_scroll_x) for a scroll container's layout box.
    Zero on an axis whose content fits (or which does not scroll)."""
    node = getattr(obj, "node", None)
    scrolls_y, scrolls_x = scroll_axes(node)
    if not (scrolls_y or scrolls_x):
        return (0.0, 0.0)
    right, bottom = _subtree_extent(obj)
    max_y = max(0.0, bottom - (obj.y + obj.height)) if scrolls_y else 0.0
    max_x = max(0.0, right - (obj.x + obj.width)) if scrolls_x else 0.0
    return (max_y, max_x)


def scroll_position(obj):
    """This box's current (y, x) scroll offset, clamped to its range."""
    node = getattr(obj, "node", None)
    if node is None:
        return (0.0, 0.0)
    max_y, max_x = scroll_range(obj)
    y = min(max(getattr(node, "_scroll_y", 0.0), 0.0), max_y)
    x = min(max(getattr(node, "_scroll_x", 0.0), 0.0), max_x)
    node._scroll_y, node._scroll_x = y, x
    return (y, x)


def scroll_container_by(obj, dy, dx=0.0):
    """Scroll a container. Returns True when it actually moved — the
    shells use that to decide whether the wheel event was consumed or
    should chain to the next scroller out (and finally the page)."""
    node = getattr(obj, "node", None)
    if node is None:
        return False
    max_y, max_x = scroll_range(obj)
    if max_y <= 0 and max_x <= 0:
        return False
    before = scroll_position(obj)
    node._scroll_y = min(max(before[0] + dy, 0.0), max_y)
    node._scroll_x = min(max(before[1] + dx, 0.0), max_x)
    return (node._scroll_y, node._scroll_x) != before


# `window.scrollTo(...)` arrives with this sentinel node index (the VM's
# DOC_NODE): it targets the page's own scroller, not an element.
PAGE_SCROLL_NODE = 0xFFFFFFFF


def collect_scroll_state(layout_list, page=None):
    """[(ridx, scrollTop, scrollLeft, scrollHeight, scrollWidth)] for
    every scroll container — what page JS observes on the element.

    `page` is the shell's own scroller as (top, left, height, width);
    it is reported under PAGE_SCROLL_NODE so `window.scrollY` and a
    relative `window.scrollBy` see where the page actually sits."""
    out = []
    if page is not None:
        out.append((PAGE_SCROLL_NODE,) + tuple(float(v) for v in page))
    for obj in layout_list:
        if not is_scroll_container(obj):
            continue
        ridx = getattr(getattr(obj, "node", None), "_ridx", None)
        if ridx is None:
            continue
        y, x = scroll_position(obj)
        max_y, max_x = scroll_range(obj)
        out.append((int(ridx), float(y), float(x),
                    float(obj.height + max_y), float(obj.width + max_x)))
    return out


def _scrollers_by_ridx(layout_list):
    out = {}
    for obj in layout_list:
        if not is_scroll_container(obj):
            continue
        ridx = getattr(getattr(obj, "node", None), "_ridx", None)
        if ridx is not None:
            out[int(ridx)] = obj
    return out


def _reveal(obj, viewport_height, page_scroll):
    """Scroll `obj`'s scrolling ancestors so it sits inside them, then
    report the page scroll that puts it on screen (None if it already
    is). Returns (containers_moved, page_scroll_or_None)."""
    moved = False
    # walk out through the scrollers, innermost first
    cur = getattr(obj, "parent", None)
    while cur is not None:
        if is_scroll_container(cur):
            # where the target sits inside this container right now
            dy, _dx = scrolled_ancestor_offset(obj)
            top = obj.y - dy
            delta = 0.0
            if top < cur.y:
                delta = top - cur.y
            elif top + obj.height > cur.y + cur.height:
                delta = (top + obj.height) - (cur.y + cur.height)
            if delta and scroll_container_by(cur, delta):
                moved = True
        cur = getattr(cur, "parent", None)
    dy, _dx = scrolled_ancestor_offset(obj)
    doc_top = obj.y - dy
    if doc_top < page_scroll:
        return (moved, doc_top)
    if doc_top + obj.height > page_scroll + viewport_height:
        return (moved, doc_top + obj.height - viewport_height)
    return (moved, None)


def scroll_into_view(layout_list, ridxs, viewport_height, page_scroll):
    """Bring elements into view for `el.scrollIntoView()`.

    Scrolls every scroll-container ancestor so the element's box sits
    inside it, then reports the page scroll the caller should adopt so
    the (now positioned) element is on screen. Returns
    (containers_moved, new_page_scroll_or_None)."""
    targets = {int(r[0]) if isinstance(r, (tuple, list)) else int(r)
               for r in ridxs}
    if not targets:
        return (False, None)
    moved = False
    page_scroll_out = None
    for obj in layout_list:
        ridx = getattr(getattr(obj, "node", None), "_ridx", None)
        if ridx is None or int(ridx) not in targets:
            continue
        want = page_scroll if page_scroll_out is None else page_scroll_out
        hit, new_page = _reveal(obj, viewport_height, want)
        moved = moved or hit
        if new_page is not None:
            page_scroll_out = new_page
    return (moved, page_scroll_out)


def apply_scroll_requests(layout_list, writes, into_view,
                          viewport_height, page_scroll, page_left=0.0):
    """Replay one turn's scroll requests in the order page JS made them.

    `writes` are (ridx, top, left, seq) and `into_view` are
    (ridx, seq) as drained from the VM; replaying them merged keeps
    `el.scrollIntoView(); window.scrollBy(0, -80)` (a sticky-header
    offset, the common idiom) landing where the page asked instead of
    letting whichever queue the host drains last win. Returns
    (element_moved, page_target_or_None) where the page target is
    (top, left) for the caller's own scroller."""
    ordered = [(w[3] if len(w) > 3 else 0, 0, w) for w in writes]
    ordered += [(r[1] if isinstance(r, (tuple, list)) and len(r) > 1 else 0,
                 1, r) for r in into_view]
    if not ordered:
        return (False, None)
    ordered.sort(key=lambda e: (e[0], e[1]))

    scrollers = _scrollers_by_ridx(layout_list)
    by_ridx = {}
    for obj in layout_list:
        ridx = getattr(getattr(obj, "node", None), "_ridx", None)
        if ridx is not None:
            by_ridx.setdefault(int(ridx), obj)

    moved = False
    page, page_x = float(page_scroll), float(page_left)
    page_moved = False
    for _seq, kind, req in ordered:
        if kind == 0:
            ridx, top, left = int(req[0]), float(req[1]), float(req[2])
            # a relative request (scrollBy) carries a delta: the VM
            # cannot know where the scroller ended up after an earlier
            # scrollIntoView this same turn, so it is resolved here
            relative = len(req) > 4 and bool(req[4])
            if ridx == PAGE_SCROLL_NODE:
                page = max((page + top) if relative else top, 0.0)
                page_x = max((page_x + left) if relative else left, 0.0)
                page_moved = True
                continue
            obj = scrollers.get(ridx)
            if obj is None:
                continue
            if relative:
                dy, dx = top, left
            else:
                cur_y, cur_x = scroll_position(obj)
                dy, dx = top - cur_y, left - cur_x
            if scroll_container_by(obj, dy, dx):
                moved = True
            continue
        ridx = int(req[0]) if isinstance(req, (tuple, list)) else int(req)
        obj = by_ridx.get(ridx)
        if obj is None:
            continue
        hit, new_page = _reveal(obj, viewport_height, page)
        moved = moved or hit
        if new_page is not None:
            page, page_moved = new_page, True
    return (moved, (page, page_x) if page_moved else None)


def capture_scroll_state(root):
    """{ridx: (y, x)} for every node with a non-zero scroll offset.

    The native path rebuilds the Python tree from the Rust arena on
    every DOM-changing tick, so brand-new Element objects would lose
    the offsets stored on them — inner scroll positions would snap back
    to the top under any live page. The shells keep this snapshot
    across rebuilds, keyed by the stable arena index (the same trick
    `_remap_marks` uses for focus and hover)."""
    state = {}
    if root is None:
        return state
    for node in tree_to_list(root, []):
        ridx = getattr(node, "_ridx", None)
        if ridx is None:
            continue
        y = getattr(node, "_scroll_y", 0.0)
        x = getattr(node, "_scroll_x", 0.0)
        if y or x:
            state[ridx] = (y, x)
    return state


def restore_scroll_state(root, state):
    """Re-apply captured offsets onto a freshly exported tree."""
    if not state or root is None:
        return
    for node in tree_to_list(root, []):
        hit = state.get(getattr(node, "_ridx", None))
        if hit is not None:
            node._scroll_y, node._scroll_x = hit


def scrolled_ancestor_offset(layout_obj):
    """Total (dy, dx) the ancestor scrollers shift this box by — the
    hit-test counterpart of the paint-time translation."""
    dy = dx = 0.0
    cur = getattr(layout_obj, "parent", None)
    while cur is not None:
        if is_scroll_container(cur):
            y, x = scroll_position(cur)
            dy += y
            dx += x
        cur = getattr(cur, "parent", None)
    return dy, dx


def find_scrollable(layout_obj, dy, dx=0.0):
    """Nearest scroller at or above `layout_obj` that can still move in
    the requested direction (scroll chaining): a wheel inside a
    bottomed-out inner box keeps scrolling the page."""
    cur = layout_obj
    while cur is not None:
        if is_scroll_container(cur):
            max_y, max_x = scroll_range(cur)
            y, x = scroll_position(cur)
            if (dy > 0 and y < max_y) or (dy < 0 and y > 0) \
                    or (dx > 0 and x < max_x) or (dx < 0 and x > 0):
                return cur
        cur = getattr(cur, "parent", None)
    return None


def hit_test_at(layout_list, x, y, page_scroll):
    """Deepest painted box under a point in document coordinates.

    A box paints where layout put it, shifted by its sticky ancestors
    and by every scroll container it sits inside — hit-testing has to
    compose the same offsets or clicks inside a scrolled area land on
    the wrong node."""
    hit = None
    for obj in layout_list:
        dy, dx = scrolled_ancestor_offset(obj)
        oy = obj.y + sticky_offset(obj, page_scroll) - dy
        ox = obj.x - dx
        if ox <= x < ox + obj.width and oy <= y < oy + obj.height:
            hit = obj
    return hit


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


def _measure_min_content_width(node, doc):
    """Min-content (border-box) main size of `node`: the same trial
    layout as the max-content one, but under a zero-width constraint so
    every break opportunity is taken and the widest unbreakable run is
    what comes back."""
    cache = getattr(doc, "_min_width_cache", None)
    if cache is None:
        cache = doc._min_width_cache = {}
    hit = cache.get(id(node))
    if hit is not None:
        return hit
    holder = _MeasureHolder(doc, 0.0)
    box = BlockLayout(node, holder, None)
    saved, saved_m = doc.abs_queue, getattr(doc, "_measuring", False)
    doc.abs_queue = []
    doc._measuring = True
    try:
        box.layout()
    finally:
        doc.abs_queue = saved
        doc._measuring = saved_m
    origin, m = box.x, 0.0
    stack = list(box.children)
    while stack:
        b = stack.pop()
        if isinstance(b, (TextLayout, ImageLayout, InlineBlockLayout)):
            w = getattr(b, "width", 0)
            tail = getattr(b, "word", None)
            if tail and tail[-1:] in BREAK_SPACE:
                trimmed = hang_trim(tail)
                w = 0.0 if not trimmed else measure(
                    cached_font(b.node), trimmed)
            m = max(m, (b.x + w) - origin)
        else:
            stack.extend(getattr(b, "children", []))
    m += box.pl + box.pr + box.bl + box.br
    cache[id(node)] = m
    return m


_FACE_OFF_BACKGROUND = frozenset((
    "background", "background-attachment", "background-clip",
    "background-color", "background-image", "background-origin",
    "background-position", "background-position-x",
    "background-position-y", "background-size",
))


def disables_native_appearance(prop):
    """Does styling `prop` turn off a form control's native widget?

    CSS UI 4 leaves the list to the UA and every engine settled on the
    same one; WPT's compute-kind-widget suite enumerates it. It is the
    background longhands plus every border colour, style, width,
    radius and border-image longhand — physical and logical alike.
    Touching any part of a control's box means the page has taken over
    drawing that box, so the native face steps aside.
    """
    if prop in _FACE_OFF_BACKGROUND:
        return True
    if not prop.startswith("border"):
        return False
    return (prop == "border" or prop.startswith("border-image-")
            or prop.endswith(("-color", "-style", "-width", "-radius")))


def intrinsic_width_keyword(raw):
    """`min-content`, `max-content` or `fit-content` out of a width
    value, or None. These are sizes only the content can answer, so
    parse_size returns None for them and the box has to ask."""
    v = (raw or "").strip().casefold()
    return v if v in ("min-content", "max-content", "fit-content") else None


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
            # a trailing breaking space hangs, so it is not part of the
            # intrinsic size either: a box holding only U+3000 is
            # min-content zero wide, not one ideograph wide
            w = getattr(b, "width", 0)
            tail = getattr(b, "word", None)
            if tail and tail[-1:] in BREAK_SPACE:
                trimmed = hang_trim(tail)
                w = 0.0 if not trimmed else measure(
                    cached_font(b.node), trimmed)
            m = max(m, (b.x + w) - origin)
        elif isinstance(b, InlineBlockLayout):
            m = max(m, b.x + b.width - origin)
        elif isinstance(b, BlockLayout):
            raw_width = getattr(b.node, "style", {}).get(
                "width", "").strip().casefold()
            # A percentage (including calc() containing one) has no
            # definite intrinsic size. Absolute/em/rem/px lengths do.
            definite_width = bool(raw_width) \
                and "%" not in raw_width \
                and "vw" not in raw_width \
                and "vh" not in raw_width \
                and parse_size(
                    raw_width, 0.0,
                    parse_px(b.node.style.get("font-size", "16px"), 16.0)
                ) is not None
            if getattr(b, "forced_width", None) is None \
                    and not definite_width:
                stack.extend(getattr(b, "children", []))
                continue
            # Definite block/flex/grid/inline-block descendants contribute
            # their complete margin box, including padding after the last
            # glyph. Counting only text/image leaves made intrinsic flex
            # rows too narrow: Naver's auto-width <li> contains a 64px
            # block link, so the flex item must also reserve 64px.
            left = b.x - b.pl - b.bl - b.ml
            m = max(m, left + b.outer_width() - origin)
        stack.extend(getattr(b, "children", []))
    result = m + box.pl + box.pr + box.bl + box.br
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
    result = m + box.pl + box.pr + box.bl + box.br
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
        # The document box is the viewport itself: origin (0,0), full
        # width. The default page inset comes from the UA `body{margin:8px}`
        # rule (style.py) — matching real browsers — not a hardcoded
        # document padding, which would double the margin (and shift every
        # coordinate vs Chrome, breaking pages that reset body margin).
        self.width = width
        self.x = 0
        self.y = 0
        self.viewport_width = width
        self.viewport_height = height
        self.definite_height = (max(height, 0)
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
            node, cb, static_x, static_y, abs_clips, abs_transforms = entry
            if id(node) in processed:
                continue
            processed.add(id(node))

            st = node.style
            # position:fixed is laid out relative to the viewport, not a
            # positioned ancestor or the whole (tall) page — so top:0 pins
            # to the top of the first screen and bottom:0 to the bottom of
            # the viewport (best a non-scrolling full-page render can do).
            grid_area = False
            if st.get("position") == "fixed":
                cb_x, cb_y = 0.0, 0.0
                cb_w = self.viewport_width or self.width
                cb_h = self.viewport_height or self.height
            elif isinstance(cb, BlockLayout):
                # A grid child's containing block is its grid area, when
                # the grid is the positioned ancestor that would have
                # supplied one anyway.
                area = getattr(node, "_grid_cb", None)
                if area is not None and cb.node is getattr(
                        node, "parent", None):
                    cb_x, cb_y, cb_w, cb_h = area
                    grid_area = True
                else:
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
            box = BlockLayout(node, self, None)
            box._pct_height_base = cb_h
            # fixed boxes are viewport-anchored: ancestor clips don't cut
            box._abs_clips = \
                [] if st.get("position") == "fixed" else abs_clips
            # The absolute pass reparents this box directly below the
            # document. Keep visual transforms from its former ancestor
            # chain so painting still moves the whole CSS subtree together.
            box._abs_transforms = \
                [] if st.get("position") == "fixed" else abs_transforms
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
            # The vertical twin (CSS 2.1 §10.6.4): an auto height with
            # both offsets given is over-constrained the useful way —
            # the box stretches between them. Without this a
            # `top:0; bottom:0; height:auto` overlay was content-tall.
            # An aspect ratio outranks the stretch: with a ratio and a
            # resolved inline size the block size comes from the ratio
            # and `bottom` is the over-constrained offset (CSS Sizing 4
            # §4), so a 1/1 box inset to 0 in a 100x500 block is a
            # 100px square rather than a 100x500 column.
            if auto_size(st, "height") \
                    and top is not None and bottom is not None \
                    and not _parse_aspect_ratio(st.get("aspect-ratio")):
                box.forced_height = max(cb_h - top - bottom, 0.0)
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
                border_box_w = (box.bl + box.pl + box.width
                                + box.pr + box.br)
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

            # vertical twin: `inset:0; margin:auto` centers a fixed-size
            # box both ways (naver's paging-arrow sprite)
            mt_auto = (st.get("margin-top") or "").strip() == "auto"
            mb_auto = (st.get("margin-bottom") or "").strip() == "auto"
            if mt_auto or mb_auto:
                border_box_h = (box.bt + box.pt + box.height
                                + box.pb + box.bb)
                if top is not None and bottom is not None:
                    leftover_v = max(
                        cb_h - top - bottom - border_box_h, 0.0)
                else:
                    leftover_v = 0.0
                if mt_auto and mb_auto:
                    box.margin_top = leftover_v / 2
                    box.margin_bottom = leftover_v / 2
                elif mt_auto:
                    box.margin_top = leftover_v
                else:
                    box.margin_bottom = leftover_v

            # An out-of-flow child of a flex or grid container does not
            # fall back to a point: its static position rectangle is the
            # container's content box, and the container's item
            # alignment places it inside that rectangle (CSS Position 3
            # §4.1, CSS Align 3 §5.2). Without this an abspos child of
            # an `align-items: center` grid sat in the top-left corner.
            sx, sy = static_x, static_y
            align_from = getattr(node, "_static_align", None)
            if grid_area:
                # With a grid area for a containing block, that area is
                # also the rectangle the item aligns in and the corner
                # an auto offset falls back to — not the whole grid.
                sx, sy = cb_x, cb_y
                if align_from is not None:
                    jmode, amode, _holder, self_inline = align_from
                    if self_inline:
                        jmode = (st.get("justify-self")
                                 or "").strip().casefold() or jmode
                    amode = (st.get("align-self")
                             or "").strip().casefold() or amode
                    sx += _static_offset(jmode, cb_w, box.outer_width())
                    sy += _static_offset(amode, cb_h, box.outer_height())
                align_from = None
            if align_from is not None:
                jmode, amode, holder, self_inline = align_from
                # the child's own -self value wins over the container's
                # -items default, the same way it would in flow
                if self_inline:
                    jmode = (st.get("justify-self")
                             or "").strip().casefold() or jmode
                amode = (st.get("align-self") or "").strip().casefold() \
                    or amode
                sx += _static_offset(
                    jmode, holder.width, box.outer_width())
                sy += _static_offset(
                    amode, holder.height, box.outer_height())

            if left is not None:
                target_x = cb_x + left
            elif right is not None:
                target_x = cb_x + cb_w - right - box.outer_width()
            else:
                target_x = sx
            if top is not None:
                target_y = cb_y + top
            elif bottom is not None:
                target_y = cb_y + cb_h - bottom - box.outer_height()
            else:
                target_y = sy
            translate(box,
                      target_x - (box.x - box.pl - box.bl - box.ml),
                      target_y - (box.y - box.pt - box.bt
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
        self.bt = self.br = self.bb = self.bl = 0
        self.forced_width = None   # border-box width imposed by flex/abs
        self.forced_height = None  # border-box height imposed by stretch
        # An out-of-flow box's percentage height resolves against its
        # containing block, which is not its layout parent — the
        # absolute pass reparents it to the document, whose definite
        # height it deliberately hides.
        self._pct_height_base = None
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
        return (self.margin_top + self.bt + self.pt + self.height
                + self.pb + self.bb + self.margin_bottom)

    def outer_width(self):
        return (self.ml + self.bl + self.pl + self.width
                + self.pr + self.br + self.mr)

    def _document(self):
        node = self.parent
        while not isinstance(node, DocumentLayout):
            node = node.parent
        return node

    def _queue_abs(self, node, static_x, static_y):
        """Queue an out-of-flow child for the positioned-layout pass,
        remembering its containing block (nearest positioned ancestor)
        and its static position (where it would sit in normal flow, used
        when an offset is auto).

        Also records the overflow-clipping ancestors at or above the
        containing block: an absolute box escapes clips BELOW its
        containing block but is still cut by any that also clip the
        containing block itself (naver's widget-board carousel keeps
        page-2's absolutely-positioned card text hidden this way)."""
        cb = _nearest_positioned(self)
        clips = []
        if cb is not None:
            cur, seen_cb = self, False
            while cur is not None and not isinstance(cur, DocumentLayout):
                if cur is cb:
                    seen_cb = True
                if seen_cb and isinstance(cur, BlockLayout) \
                        and cur._clips():
                    clips.append(cur)
                cur = getattr(cur, "parent", None)
        # Out-of-flow descendants are later reparented under the document
        # layout. Preserve transforms from the visual ancestor chain;
        # otherwise an absolute child stays at the untransformed coordinate
        # while its parent moves (Naver's search border moved to center but
        # its absolute N logo remained halfway across the field).
        transforms = list(getattr(self, "_abs_transforms", ()))
        cur = self
        while cur is not None and not isinstance(cur, DocumentLayout):
            if isinstance(cur, BlockLayout):
                tf = getattr(cur.node, "style", {}).get("transform")
                if tf and tf != "none" and cur not in transforms:
                    transforms.append(cur)
            cur = getattr(cur, "parent", None)
        self._document().abs_queue.append(
            (node, cb, static_x, static_y, clips, transforms))

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
            raw = st.get(prop)
            # only pay for the font measurement when the value is
            # actually font-relative in a way `em` cannot answer
            ch = lh = None
            if raw and ("ch" in raw or "ex" in raw):
                ch = _ch_width(node)
            if raw and "lh" in raw:
                lh = lh_unit(node, em)
            return parse_size(raw, base, em, ch, lh)

        self.pt = size("padding-top") or 0
        self.pr = size("padding-right") or 0
        self.pb = size("padding-bottom") or 0
        self.pl = size("padding-left") or 0
        self.bw = size("border-width", 0) or 0
        self.bt = (size("border-top-width", 0)
                   if "border-top-width" in st else self.bw) or 0
        self.br = (size("border-right-width", 0)
                   if "border-right-width" in st else self.bw) or 0
        self.bb = (size("border-bottom-width", 0)
                   if "border-bottom-width" in st else self.bw) or 0
        self.bl = (size("border-left-width", 0)
                   if "border-left-width" in st else self.bw) or 0
        self.margin_top = size("margin-top") or 0
        self.margin_bottom = size("margin-bottom") or 0
        ml_raw = st.get("margin-left", "0").strip()
        mr_raw = st.get("margin-right", "0").strip()
        self.ml = size("margin-left") or 0
        self.mr = size("margin-right") or 0

        mode = layout_mode(node)
        if _margin_context_allows_children(node) \
                and self.pt == 0 and self.bt == 0:
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

        edge = self.pl + self.pr + self.bl + self.br
        spec = size("width")
        maxw = size("max-width")
        minw = size("min-width")
        # box-sizing (CSS Box Sizing §3): border-box counts padding+border
        # inside a specified width, content-box adds them outside. This
        # engine defaults a bare width to border-box ("web reality": most
        # sites ship `* { box-sizing: border-box }`), and honours an
        # explicit box-sizing:content-box for the pages that opt out.
        border_box = st.get(
            "box-sizing", "content-box").strip().casefold() == "border-box"

        def to_border(v):
            return v if border_box else v + edge
        # min-content / max-content / fit-content are sizes only the
        # content can answer, so parse_size hands back None for them
        # and the box measures itself. The trial layout that answers is
        # the one box whose parent is the throwaway holder, so it takes
        # the constraint the holder imposes instead of asking again.
        # aspect-ratio, the other way round: a definite block size and
        # an auto inline size give the inline size (CSS Sizing 4 §4).
        # `height: 100px; aspect-ratio: 1/1` is a 100px square, not a
        # full-width band 100px tall. The value goes in as `spec`, so
        # box-sizing lands it on whichever box it names.
        if spec is None and self.forced_width is None:
            ratio = _parse_aspect_ratio(st.get("aspect-ratio"))
            if ratio and ratio > 0:
                from_h = self._specified_height(st, em)
                if from_h is not None:
                    spec = max(from_h, 0.0) * ratio
        intrinsic_w = None
        intrinsic = intrinsic_width_keyword(st.get("width"))
        if intrinsic and not isinstance(self.parent, _MeasureHolder):
            try:
                doc = self._document()
                if intrinsic == "min-content":
                    intrinsic_w = _measure_min_content_width(node, doc)
                else:
                    maxc = _measure_content_width(node, doc)
                    intrinsic_w = maxc if intrinsic == "max-content" \
                        else min(maxc, max(avail - self.ml - self.mr, 0.0))
            except Exception:
                intrinsic_w = None
        if intrinsic_w is not None:
            box_w = max(intrinsic_w, 0.0)      # already a border box
        elif self.forced_width is not None:
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
            self.x = base_x + self.ml + self.bl + self.pl
            self.y = base_y + self.margin_top + self.bt + self.pt
        else:
            self.x = self.parent.x + self.ml + self.bl + self.pl
            if collapsed_with_parent:
                self.y = self.parent.y + self.bt + self.pt
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
                              + self.bt + self.pt)
                else:
                    collapse = collapse_margins(
                        p.margin_bottom, self.margin_top)
                    self.y = (p.y + p.height + p.pb + p.bb + collapse
                              + self.bt + self.pt)
            else:
                self.y = (self.parent.y + self.margin_top
                          + self.bt + self.pt)
            # clear: drop below the matching floats (set by the parent)
            floor = getattr(self, "clear_y_floor", None)
            if floor is not None:
                self.y = max(
                    self.y, floor + self.margin_top + self.bt + self.pt)
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
                spec_h - (self.pt + self.pb + self.bt + self.bb)
                if border_box else spec_h, 0)
        elif self.forced_height is not None:
            # A stretched grid item: the parent hands down a border-box
            # height the same way it hands down forced_width. Only an
            # auto height gets here — an item that says how tall it is
            # is that tall, and alignment moves it instead.
            self.definite_height = max(
                self.forced_height - (self.pt + self.pb + self.bt + self.bb),
                0)

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
            for child in _effective_block_children(node):
                if not is_visible(child):
                    continue
                # Formatting whitespace between block tags creates no box
                # and must not interrupt adjoining sibling/parent margins.
                if isinstance(child, Text) and not child.text.strip():
                    continue
                if is_out_of_flow(child):
                    flow_y = self.y if previous is None else (
                        previous.y + previous.height + previous.pb
                        + previous.bb + previous.margin_bottom)
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
                    else:
                        edge = sum(
                            parse_size(child.style.get(p), self.width, cem)
                            or 0.0 for p in
                            ("padding-left", "padding-right"))
                        edge += _style_border_width(
                            child.style, "left", self.width, cem)
                        edge += _style_border_width(
                            child.style, "right", self.width, cem)
                        if child.style.get(
                                "box-sizing", "content-box").strip() \
                                .casefold() == "border-box":
                            w = max(w, edge)
                        else:
                            w += edge
                    if ib_x is None:
                        ib_row_y = self.y if previous is None else (
                            previous.y + previous.height + previous.pb
                            + previous.bb + previous.margin_bottom)
                        ib_x = 0
                        ib_row_h = 0
                    box = BlockLayout(child, self, None)
                    box.forced_width = max(w, 0.0)
                    box.flex_origin = (self.x + ib_x, ib_row_y)
                    box.layout()
                    # white-space:nowrap keeps the run on one line even
                    # past the container edge (carousel viewports: naver's
                    # widget board lines up 420px pages inside an
                    # overflow:hidden 420px wrap — page 2 must overflow
                    # rightward and be clipped, not stack below)
                    nowrap = white_space(node.style) in ("nowrap", "pre")
                    if not nowrap and ib_x > 0 \
                            and ib_x + box.outer_width() > self.width:
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
                        + previous.bb + previous.margin_bottom)
                    box = BlockLayout(child, self, None)
                    box.forced_width = fw
                    box.float_side = fside
                    flx, fly = self._float_pos(fside, cur_y, fw)
                    box.flex_origin = (flx, fly)
                    self.children.append(box)
                    box.layout()
                    self._floats.append((
                        fside,
                        box.x - box.ml - box.bl - box.pl,
                        fly,
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
                and self.pb == 0 and self.bb == 0
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
                                   + previous.pb + previous.bb)
            else:
                flow_bottom = self.y if previous is None else (
                    previous.y + previous.height + previous.pb
                    + previous.bb + previous.margin_bottom)
            float_bottom = max(
                (fy + fh for (_, _, fy, _, fh) in self._floats),
                default=self.y)
            self.height = max(flow_bottom, float_bottom) - self.y
            # A clamp on a box that ended up in block flow — the legacy
            # `display: -webkit-box` wrapping a block — counts the lines
            # of the whole subtree, so it is applied to the finished
            # tree rather than to this box's own line list.
            n_lines = line_clamp_count(st)
            if n_lines is None and line_clamp_auto(st):
                room = self._content_height_limit(st.get("max-height"), em)
                if room is None:
                    room = self.definite_height
                lh = lh_unit(node, em)
                if room is not None and lh > 0:
                    n_lines = max(int(room / lh + 1e-6), 1)
            if n_lines:
                clamp_lines(self, n_lines, block_ellipsis(st))
            apply_relative_offsets(self.children)
        else:
            self.new_line()
            self._ws_pending = False   # no space owed before the first atom
            self.recurse(node)
            # -webkit-line-clamp: N — keep the first N wrapped lines and
            # mark the overflow with an ellipsis (card titles clamp to 2
            # lines so a grid of cards stays uniform height).
            n_lines = line_clamp_count(st)
            if n_lines is None and line_clamp_auto(st):
                # `auto`: as many lines as the box's own height allows.
                # Line boxes are uniform here, so the count comes off
                # the line height rather than out of a layout pass the
                # cut below would have to redo.
                room = self._content_height_limit(st.get("max-height"), em)
                if room is None:
                    room = self.definite_height
                if room is not None:
                    lh = lh_unit(node, em)
                    if lh > 0:
                        n_lines = max(int(room / lh + 1e-6), 1)
            if n_lines and len(self.children) > n_lines:
                self.children = self.children[:n_lines]
                last = self.children[-1]
                words = [w for w in last.children
                         if isinstance(w, TextLayout)]
                if words:
                    words[-1].word = words[-1].word.rstrip() \
                        + block_ellipsis(st)
            for line in self.children:
                line.layout()
            self.height = sum(line.height for line in self.children)

        # A textarea's auto height is an intrinsic replaced-element size,
        # not a UA `height` declaration. Keeping it intrinsic lets an
        # author rule such as padding-top plus a one-sided border compose
        # exactly as it does in Chromium (Google's rows=1 search field).
        if spec_h is None and isinstance(node, Element) \
                and node.tag == "textarea":
            try:
                rows = max(int(node.attributes.get("rows", "2")), 1)
            except (TypeError, ValueError):
                rows = 2
            line_h = parse_size(st.get("line-height"), 0, em)
            if line_h is None:
                line_h = cached_font(node).gg_linespace
            self.height = max(self.height, rows * line_h + 6.0)

        if self.definite_height is not None:
            self.height = self.definite_height

        # aspect-ratio gives an auto height a definite one, derived from
        # the width the box already has. It only applies when the other
        # axis is auto — a box with both width and height specified is
        # that size and the ratio is ignored (CSS Sizing 4 §4).
        if spec_h is None and self.definite_height is None:
            ratio = _parse_aspect_ratio(st.get("aspect-ratio"))
            if ratio and ratio > 0:
                # the ratio sizes whichever box box-sizing names, so
                # padding and border join the ratio under border-box and
                # sit outside it under the content-box default
                if border_box:
                    from_ratio = (
                        self.bl + self.pl + self.width + self.pr + self.br
                    ) / ratio - (self.pt + self.pb + self.bt + self.bb)
                else:
                    from_ratio = self.width / ratio
                from_ratio = max(from_ratio, 0.0)
                # min-height's initial value is auto, and for a box with
                # a preferred aspect ratio auto is the content-based
                # minimum — so a ratio that would cut the content short
                # loses to the content instead of clipping it. An
                # explicit min-height (0 included) says otherwise.
                raw_min = (st.get("min-height") or "auto").strip().casefold()
                # ...and a scroll container has no automatic minimum at
                # all: `overflow: hidden` means the content is meant to
                # be clipped, so the ratio holds and the overflow goes
                # where the author put it
                scrolls = any(
                    (st.get(prop) or "visible").strip().casefold()
                    not in ("visible", "clip")
                    for prop in ("overflow", "overflow-y"))
                if raw_min == "auto" and not scrolls:
                    from_ratio = max(from_ratio, self.height)
                self.height = from_ratio

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
                    + self.previous.pb + self.previous.bb)
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
        if "vh" in raw or "vw" in raw:
            doc = self._document()
            # Mixed vw/vh calc() expressions need a two-base evaluator;
            # until then accept the common single-viewport-unit form.
            if "vh" in raw and "vw" in raw:
                return None
            base = (doc.viewport_height if "vh" in raw
                    else doc.viewport_width)
            return parse_size(raw, base, em) if base is not None else None
        if "%" in raw:
            base = self._pct_height_base
            if base is None:
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
        if "vh" in raw or "vw" in raw:
            doc = self._document()
            if "vh" in raw and "vw" in raw:
                return None
            base = (doc.viewport_height if "vh" in raw
                    else doc.viewport_width)
            box = parse_size(raw, base, em) if base is not None else None
        elif "%" in raw:
            base = self._pct_height_base
            if base is None:
                base = self.parent.definite_height
            box = parse_size(raw, base, em) if base is not None else None
        else:
            box = parse_size(raw, 0, em, None,
                             lh_unit(self.node, em) if "lh" in raw else None)
        if box is None:
            return None
        return max(box - self.pt - self.pb - self.bt - self.bb, 0.0)

    def _float_pos(self, side, y, w):
        """(x, y) for a new float: packed after the floats already at
        this y; when it no longer fits between the float edges it drops
        below the shortest blocking float and retries (CSS 2.1 §9.5.1
        rules 4/7 — this wrap is what turns naver's 24 float:left
        16.66% press boxes into a 6×4 logo grid instead of one clipped
        row)."""
        left_edge = self.x
        right_edge = self.x + self.width
        for _ in range(len(self._floats) + 1):
            left_edge = self.x
            right_edge = self.x + self.width
            blockers = []
            for (s, fx, fy, fw, fh) in self._floats:
                if not (fy <= y < fy + fh):
                    continue
                blockers.append((fy, fh))
                if s == "left":
                    left_edge = max(left_edge, fx + fw)
                else:
                    right_edge = min(right_edge, fx)
            if not blockers or w <= right_edge - left_edge + 0.5:
                break
            y = min(fy + fh for (fy, fh) in blockers)
        if side == "left":
            return left_edge, y
        return max(right_edge - w, left_edge), y

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
            translate(b, 0, y - (b.y - b.margin_top - b.bt - b.pt))
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
                # a flex item has no justify-self — the main axis is
                # the container's justify-content and nothing else, so
                # the child's own value must not be consulted
                child._static_align = (
                    (node.style.get("justify-content") or "").strip()
                    .casefold(),
                    (node.style.get("align-items") or "").strip().casefold(),
                    self, False)
                self._queue_abs(child, self.x, self.y)
                continue
            if isinstance(child, Text) and not child.text.strip():
                continue
            kid_nodes.append(child)
        kid_nodes = order_items(kid_nodes)

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
        margin_extras = []
        for child in kid_nodes:
            cem = parse_px(child.style.get("font-size", "16px"), 16.0)
            edge = sum(
                parse_size(child.style.get(p), self.width, cem) or 0.0
                for p in ("padding-left", "padding-right"))
            edge += _style_border_width(
                child.style, "left", self.width, cem)
            edge += _style_border_width(
                child.style, "right", self.width, cem)
            child_border_box = child.style.get(
                "box-sizing", "content-box").strip().casefold() \
                == "border-box"

            def to_border_size(value):
                if value is None:
                    return None
                return max(value, edge) if child_border_box \
                    else value + edge

            basis = child.style.get("flex-basis", "").strip().casefold()
            if basis and basis not in ("auto", "content", "max-content",
                                       "fit-content", "min-content"):
                base = parse_size(basis, self.width, em)
            else:
                base = None
            if base is None:
                base = parse_size(child.style.get("width"), self.width, em)
            if base is None:
                base = _measure_content_width(child, doc)
            else:
                base = to_border_size(base)
            # min-width floors the base size (a pill with min-width:75px
            # must reserve that much of the row)
            minw = parse_size(child.style.get("min-width"),
                              self.width, em)
            if minw is not None:
                base = max(base or 0.0, to_border_size(minw))
            specs.append(max(base or 0.0, 0.0))
            margin_extras.append(sum(
                parse_size(child.style.get(p), self.width, cem) or 0.0
                for p in ("margin-left", "margin-right")
                if child.style.get(p, "").strip().casefold() != "auto"))

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

        # flex-grow defaults to zero. Earlier versions made auto-basis
        # children divide all leftover width, which looked convenient for
        # naive equal-column demos but is contrary to Flexbox and expands
        # real headers/footers to the 100000px intrinsic-measure sentinel.
        # Intrinsic max-content measurement also never distributes free
        # space: it asks how large the contents are, not how large a flexed
        # item can become in an arbitrary wide container.
        measuring = bool(getattr(doc, "_measuring", False))
        eff_grows = [0.0] * len(grows) if measuring else grows
        eff_total = sum(eff_grows)

        gap_total = col_gap * max(len(specs) - 1, 0)
        fixed_margins = sum(margin_extras)
        n = len(specs)
        if not wrap:
            free = self.width - sum(specs) - fixed_margins - gap_total
            if free > 0 and eff_total > 0:
                # grow: distribute positive free space, clamp each item to
                # its max-width, freeze it, and redistribute the remainder
                maxes = []
                for c in kid_nodes:
                    c_em = parse_px(c.style.get("font-size", "16px"), 16.0)
                    maximum = parse_size(
                        c.style.get("max-width"), self.width, c_em)
                    if maximum is not None and c.style.get(
                            "box-sizing", "content-box").strip().casefold() \
                            != "border-box":
                        maximum += sum(
                            parse_size(c.style.get(p), self.width, c_em)
                            or 0.0 for p in
                            ("padding-left", "padding-right"))
                        maximum += _style_border_width(
                            c.style, "left", self.width, c_em)
                        maximum += _style_border_width(
                            c.style, "right", self.width, c_em)
                    maxes.append(maximum)
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
        fixed_total = sum(specs) + fixed_margins

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
        if n_auto and not measuring:
            free = self.width - fixed_total - grow * n_flex - gap_total
            auto_px = max(free / n_auto, 0.0)

        cx, row_y, row_h = self.x, self.y, 0.0
        rows = []      # [(boxes, row_height)]
        row_boxes = []
        for item_index, (child, spec) in enumerate(zip(kid_nodes, specs)):
            w = spec if spec is not None else grow
            gap_before = col_gap if row_boxes else 0.0
            if wrap and cx + gap_before + w \
                    + margin_extras[item_index] > self.x + self.width \
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

        # align-content: a flex container with a definite height has
        # cross-axis room its lines have not used. The default is
        # `stretch`, which is why align-items:center inside a 100px-tall
        # flex row was centring inside the 20px line instead of the
        # container — the line has to grow first, and nothing grew it.
        if self.definite_height is not None and rows and not measuring:
            used = (sum(h for _b, h in rows)
                    + row_gap * max(len(rows) - 1, 0))
            free = self.definite_height - used
            content = (node.style.get("align-content")
                       or "").strip().casefold()
            if free > 0:
                if content in ("", "normal", "stretch"):
                    grow_each = free / len(rows)
                    shifted = 0.0
                    stretched = []
                    for boxes, height in rows:
                        if shifted:
                            for b in boxes:
                                translate(b, 0, shifted)
                        stretched.append((boxes, height + grow_each))
                        shifted += grow_each
                    rows = stretched
                else:
                    lead, spread = distribute_free_space(
                        free, len(rows), content)
                    if lead or spread:
                        for i, (boxes, _h) in enumerate(rows):
                            for b in boxes:
                                translate(b, 0, lead + spread * i)
            self.height = self.definite_height

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
            if not measuring and justify not in (
                    "", "flex-start", "start", "normal",
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
                        - b.bt - b.bb - b.pt - b.pb
                    if fill > b.height:
                        b.height = fill
                # flex-start / baseline: top edge, no change
        self._flex_gap_rules(node, em)
        apply_relative_offsets(self.children)

    def _flex_gap_rules(self, node, em):
        """Gap decorations for a flex container, read back off the laid
        out items: a rule down the middle of every gap between two
        items on a line, and across every gap between two lines. Flex
        has no track grid to consult, so the boxes are the record of
        where the gaps ended up."""
        col_rule = gap_rule(node.style, "column", em)
        row_rule = gap_rule(node.style, "row", em)
        self._gap_rules = rules = []
        if not (col_rule or row_rule) or not self.children:
            return

        def edges(b):
            left = b.x - b.pl - b.bl - b.ml
            top = b.y - b.pt - b.bt - b.margin_top
            return (left, top, left + b.outer_width(), top + b.outer_height())

        lines = {}
        for b in self.children:
            lines.setdefault(round(edges(b)[1], 1), []).append(b)
        keys = sorted(lines)
        if col_rule:
            w, rgb = col_rule
            for key in keys:
                row = sorted(lines[key], key=lambda b: edges(b)[0])
                for prev, nxt in zip(row, row[1:]):
                    mid = (edges(prev)[2] + edges(nxt)[0]) / 2.0
                    top = min(edges(b)[1] for b in row)
                    bot = max(edges(b)[3] for b in row)
                    rules.append((mid - w / 2, top, mid + w / 2, bot, rgb))
        if row_rule:
            w, rgb = row_rule
            for a, b in zip(keys, keys[1:]):
                mid = (max(edges(x)[3] for x in lines[a])
                       + min(edges(x)[1] for x in lines[b])) / 2.0
                rules.append((self.x, mid - w / 2,
                              self.x + self.width, mid + w / 2, rgb))

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
                - box.bt - box.bb - box.pt - box.pb
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
        # gaps first: repeat(auto-fill, ...) counts how many copies of its
        # pattern fit, which it cannot do without knowing the gutter
        col_gap = parse_size(
            style.get("column-gap") or style.get("grid-column-gap")
            or self._gap_shorthand(style, 1), self.width, em) or 0.0
        row_gap = parse_size(
            style.get("row-gap") or style.get("grid-row-gap")
            or self._gap_shorthand(style, 0), self.width, em) or 0.0

        tracks, col_lines = _parse_grid_template(
            cols_spec, self.width, em, col_gap)
        explicit_cols = len(tracks)
        if not tracks:
            # No template: the one implicit column is sized by
            # grid-auto-columns, whose initial value is `auto`. An auto
            # track still stretches into the container, so a bare
            # `display: grid` looks the same — but unlike the 1fr this
            # used, it carries the items' min-content floor, so a grid
            # narrower than its content stops clipping the item's box
            # to the track.
            tracks = [("intrinsic", "min", "auto")]
        ncols = len(tracks)
        row_tracks, row_lines = _parse_grid_template(
            rows_spec, self.definite_height or self.width, em, row_gap)

        place = style.get("place-items", "").split()
        align_items = (style.get("align-items")
                       or (place[0] if place else "stretch")) \
            .strip().casefold()
        justify_items = (style.get("justify-items")
                         or (place[1] if len(place) > 1
                             else place[0] if place else "stretch")) \
            .strip().casefold()

        items, out_of_flow = [], []
        for child in node.children:
            if not (isinstance(child, Element) and is_visible(child)):
                continue
            if is_out_of_flow(child):
                child._static_align = (
                    justify_items, align_items, self, True)
                self._queue_abs(child, self.x, self.y)
                out_of_flow.append(child)
                continue
            items.append(child)
        items = order_items(items)

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

        # Implicit columns (CSS Grid 1 §8.5). A placement that reaches
        # past the last explicit line creates the tracks it needs
        # instead of being clamped back inside the explicit grid;
        # grid-auto-columns sizes them, cycling if it lists several.
        # Negative line numbers stay resolved against the explicit
        # grid, which is why this grows only after placement is read.
        wanted = max([c0 + cs for _c, c0, cs, _r, _rs in specs
                      if c0 is not None] + [ncols])
        auto_cols = _parse_grid_tracks(
            style.get("grid-auto-columns", ""), self.width, em, col_gap)
        if wanted > explicit_cols and auto_cols:
            tracks = tracks[:explicit_cols] + [
                auto_cols[i % len(auto_cols)]
                for i in range(wanted - explicit_cols)]
            ncols = len(tracks)
        elif wanted > ncols:
            tracks = tracks + [("intrinsic", "min", "auto")] * (wanted - ncols)
            ncols = len(tracks)

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

        # Column widths can only be resolved now: a content-sized track
        # is sized by the items that landed in it, and which items those
        # are is what placement just decided.
        needs_content = any(t[0] in ("intrinsic", "frmin") for t in tracks)
        contributions = [(0.0, 0.0)] * ncols
        if needs_content:
            demand = [[0.0, 0.0] for _ in range(ncols)]
            for child, c0, cspan, _r0, _rs in placed:
                try:
                    lo = _measure_min_width(child, doc)
                    hi = _measure_content_width(child, doc)
                except Exception:
                    continue
                spec_w = parse_size(child.style.get("width"), 0.0, em)
                if spec_w is not None:
                    lo = hi = spec_w
                # a spanning item is spread evenly over its tracks; the
                # full algorithm distributes only the excess over what
                # the spanned tracks already ask for, which needs a
                # second pass this does not do
                for c in range(c0, min(c0 + cspan, ncols)):
                    demand[c][0] = max(demand[c][0], lo / cspan)
                    demand[c][1] = max(demand[c][1], hi / cspan)
            contributions = [tuple(d) for d in demand]
        # justify-content decides whether leftover space stretches the
        # auto tracks or is left as a gap the alignment then uses
        jc = (style.get("justify-content") or "").strip().casefold()
        col_w = _size_grid_tracks(
            tracks, self.width, col_gap, contributions,
            stretch=jc in ("", "normal", "stretch"))
        lead, spread = distribute_free_space(
            self.width - (sum(col_w) + col_gap * max(ncols - 1, 0)),
            ncols, jc)
        col_x = []
        cx = lead
        for i in range(ncols):
            col_x.append(cx)
            cx += col_w[i] + col_gap + spread

        nrows = max(len(row_tracks), len(areas),
                    max((p[3] + p[4] for p in placed), default=0))
        row_h = [0.0] * nrows
        for i, track in enumerate(row_tracks):
            if track[0] == "fixed":
                row_h[i] = track[1]
            elif track[0] == "frmin" and not isinstance(track[2], str):
                row_h[i] = float(track[2])       # minmax(<len>, <n>fr)
            elif track[0] == "intrinsic" and not isinstance(track[1], str):
                row_h[i] = float(track[1])       # minmax(<len>, ...)
        for p in placed:
            child, c0, cspan, r0, rspan = p
            w = sum(col_w[c0:c0 + cspan]) + col_gap * (cspan - 1)
            box = BlockLayout(child, self, None)
            justify_self = (child.style.get("justify-self", "").strip()
                            .casefold() or justify_items)
            width_is_auto = not child.style.get("width") \
                or child.style.get("width", "").strip().casefold() == "auto"
            if justify_self in ("stretch", "normal", "auto", "") \
                    and width_is_auto:
                box.forced_width = max(w, 0.0)
            else:
                specified = parse_size(child.style.get("width"), w, em)
                if specified is None:
                    try:
                        specified = _measure_content_width(child, doc)
                    except Exception:
                        specified = w
                box.forced_width = max(min(specified, w), 0.0)
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

        # Flexible rows consume a definite grid container's remaining
        # block size. This turns minmax(0,1fr) into the full 160px Google
        # logo track instead of its 92px content minimum.
        if self.definite_height is not None and nrows:
            fr_rows = [i for i, track in enumerate(row_tracks)
                       if track[0] in ("fr", "frmin")]
            if fr_rows:
                fixed_h = sum(row_h[i] for i in range(nrows)
                              if i not in fr_rows)
                available = max(
                    self.definite_height - row_gap * (nrows - 1) - fixed_h,
                    0.0)
                fr_total = sum(row_tracks[i][1] for i in fr_rows)
                if fr_total > 0:
                    for i in fr_rows:
                        row_h[i] = max(
                            row_h[i],
                            available * row_tracks[i][1] / fr_total)

        row_y = [0.0] * nrows
        ac = (style.get("align-content") or "").strip().casefold()
        used_h = sum(row_h) + row_gap * max(nrows - 1, 0)
        if self.definite_height is not None \
                and ac in ("", "normal", "stretch") \
                and self.definite_height > used_h:
            # CSS Grid 1 §12.8 again, on the block axis: with no
            # explicit distribution the auto rows share the container's
            # leftover height. Without this an `align-items: center`
            # item centres inside its own content height, which is to
            # say it does not move.
            auto_rows = [r for r in range(nrows)
                         if r >= len(row_tracks)
                         or row_tracks[r][0] == "intrinsic"]
            if auto_rows:
                grow_each = (self.definite_height - used_h) / len(auto_rows)
                for r in auto_rows:
                    row_h[r] += grow_each
                used_h = sum(row_h) + row_gap * max(nrows - 1, 0)
        v_lead, v_spread = distribute_free_space(
            (self.definite_height - used_h)
            if self.definite_height is not None else 0.0, nrows, ac)
        y = v_lead
        for r in range(nrows):
            row_y[r] = y
            y += row_h[r] + row_gap + v_spread
        total_h = (y - row_gap - v_spread) if nrows else 0.0

        # An absolutely positioned child of a *positioned* grid takes
        # its grid area as its containing block, not the grid's padding
        # box (CSS Grid 1 §9). Each auto side of that area falls back
        # to the padding edge, so `grid-column: 5 / 7` with no row named
        # is a full-height band over columns five and six.
        for child in out_of_flow:
            c0, c1, _sr, _er = self._grid_lines(
                child, "column", ncols, col_lines)
            r0, r1, _sr, _er = self._grid_lines(
                child, "row", nrows, row_lines)

            x0 = (self.x + col_x[min(c0, ncols - 1)]) \
                if c0 is not None and ncols else None
            x1 = None
            if c1 is not None and ncols:
                j = min(max(c1, 1), ncols)
                x1 = self.x + col_x[j - 1] + col_w[j - 1]
            y0 = (self.y + row_y[min(r0, nrows - 1)]) \
                if r0 is not None and nrows else None
            y1 = None
            if r1 is not None and nrows:
                j = min(max(r1, 1), nrows)
                y1 = self.y + row_y[j - 1] + row_h[j - 1]
            if x0 is None and x1 is None and y0 is None and y1 is None:
                child._grid_cb = None
                continue
            pad_x0, pad_y0 = self.x - self.pl, self.y - self.pt
            pad_x1 = self.x + self.width + self.pr
            pad_y1 = (self.y + (self.definite_height
                                if self.definite_height is not None
                                else total_h) + self.pb)
            x0 = pad_x0 if x0 is None else x0
            x1 = pad_x1 if x1 is None else x1
            y0 = pad_y0 if y0 is None else y0
            y1 = pad_y1 if y1 is None else y1
            child._grid_cb = (min(x0, x1), min(y0, y1),
                              abs(x1 - x0), abs(y1 - y0))

        self.children = []
        for child, c0, cspan, r0, rspan, box in placed:
            area_w = sum(col_w[c0:c0 + cspan]) \
                + col_gap * (cspan - 1)
            area_h = sum(row_h[r0:r0 + rspan]) \
                + row_gap * (rspan - 1)
            justify_self = (child.style.get("justify-self", "").strip()
                            .casefold() or justify_items)
            align_self = (child.style.get("align-self", "").strip()
                          .casefold() or align_items)
            ml_auto = child.style.get("margin-left", "").strip() == "auto"
            mr_auto = child.style.get("margin-right", "").strip() == "auto"
            mt_auto = child.style.get("margin-top", "").strip() == "auto"
            mb_auto = child.style.get("margin-bottom", "").strip() == "auto"
            # `align-self`'s initial `normal` is stretch on a grid item
            # (CSS Grid 1 §6.2): an auto height fills the row area. An
            # empty item was coming out zero-tall, so a track it had
            # been given painted nothing at all.
            stretch_y = (align_self in ("stretch", "normal", "auto", "")
                         and not mt_auto and not mb_auto
                         and auto_size(child.style, "height"))
            if stretch_y:
                box.forced_height = max(
                    area_h - box.margin_top - box.margin_bottom, 0.0)
            free_x = max(area_w - box.outer_width(), 0.0)
            free_y = (0.0 if stretch_y
                      else max(area_h - box.outer_height(), 0.0))
            # auto margins eat the free space before alignment gets it
            if ml_auto or mr_auto:
                dx = (free_x / 2 if ml_auto and mr_auto
                      else free_x if ml_auto else 0.0)
            else:
                dx = _static_offset(justify_self, area_w, area_w - free_x)
            if mt_auto or mb_auto:
                dy = (free_y / 2 if mt_auto and mb_auto
                      else free_y if mt_auto else 0.0)
            else:
                dy = _static_offset(align_self, area_h, area_h - free_y)
            box.flex_origin = (
                self.x + col_x[c0] + dx,
                self.y + row_y[r0] + dy)
            box.layout()
            self.children.append(box)
        self.height = max(
            self.definite_height if self.definite_height is not None
            else total_h, 0.0)
        # Gap decorations: a rule down the middle of every gap, spanning
        # the container's content box in the other axis.
        rules = []
        col_rule = gap_rule(style, "column", em)
        if col_rule and col_gap > 0:
            w, rgb = col_rule
            for i in range(ncols - 1):
                mid = self.x + (col_x[i] + col_w[i] + col_x[i + 1]) / 2.0
                rules.append((mid - w / 2, self.y, mid + w / 2,
                              self.y + self.height, rgb))
        row_rule = gap_rule(style, "row", em)
        if row_rule and row_gap > 0:
            w, rgb = row_rule
            for i in range(nrows - 1):
                mid = self.y + (row_y[i] + row_h[i] + row_y[i + 1]) / 2.0
                rules.append((self.x, mid - w / 2,
                              self.x + self.width, mid + w / 2, rgb))
        self._gap_rules = rules
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
        s, e, start_ref, end_ref = self._grid_lines(
            child, axis, track_count, line_names)
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

    def _grid_lines(self, child, axis, track_count, line_names):
        """``(start_line, end_line, start_ref, end_ref)`` for one axis.

        Either line is None when that side is `auto`. Auto-placement
        collapses both cases into a span, but an absolutely positioned
        grid child does not: an auto side there means the grid
        container's own padding edge (CSS Grid 1 §9), which is a
        different rectangle from "one track wide".
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

        return s, e, start_ref, end_ref

    def _grid_column(self, child, ncols):
        """Backward-compatible column placement helper."""
        return self._grid_axis(child, "column", ncols, {})

    # ----- inline layout -----

    def new_line(self):
        self.cursor_x = 0
        last = self.children[-1] if self.children else None
        line = LineLayout(self.node, self, last)
        indent, hanging, each_line = text_indent_px(
            self.node.style, self.width,
            parse_px(self.node.style.get("font-size", "16px"), 16.0))
        # which lines the indent lands on: the first one, or with
        # `each-line` the first after every forced break too, and
        # `hanging` swaps the answer for every line
        indented = last is None or (
            each_line and getattr(self, "_forced_break", False))
        if indent and indented != hanging:
            line.indent = indent
            self.cursor_x = indent
        self.children.append(line)

    def recurse(self, node):
        if isinstance(node, Text):
            ws = white_space(node.style)
            if ws == "pre":
                self.preformatted(node)
            elif ws in ("pre-wrap", "pre-line", "break-spaces"):
                # all three honor source newlines and still wrap;
                # pre-line collapses runs of whitespace, pre-wrap
                # preserves them, break-spaces preserves them and may
                # also break inside one
                self.pre_wrapped(node, collapse=(ws == "pre-line"),
                                 break_spaces=(ws == "break-spaces"))
            else:
                # collapse whitespace, but remember whether the source had
                # any at each boundary so a space is inserted only where one
                # existed: "<b>a</b><b>b</b>" -> "ab", "$<b>5</b>" -> "$5",
                # while "hello <b>world</b>" keeps its space.
                text = node.text
                toks = css_words(text)
                if not toks:
                    if text:            # whitespace-only node: owes a space
                        self._ws_pending = True
                else:
                    lead = leads_with_space(text)
                    for i, word in enumerate(toks):
                        sb = (i > 0) or lead \
                            or getattr(self, "_ws_pending", False)
                        self.word(node, word, space_before=sb)
                        self._ws_pending = False
                    self._ws_pending = ends_with_space(text)
        else:
            if not is_visible(node):
                return
            # queue only out-of-flow *descendants*; self.node is the box
            # already being laid out (it was pulled off abs_queue), so its
            # own children must flow here instead of re-queuing it
            if node is not self.node and is_out_of_flow(node):
                line = self.children[-1] if self.children else None
                # A line's y is only real once it has been laid out, and
                # the inline walk that queues this box runs before that.
                # Falling back to the box's own top edge keeps an
                # out-of-flow child of an inline-formatting block at its
                # static position instead of the page origin.
                sy = line.y if line is not None and line.y else self.y
                self._queue_abs(node, self.x + self.cursor_x, sy)
                return
            if node is self.node and node.tag in REPLACED_CONTROLS:
                # a replaced control paints its own face (selected option
                # label, typed textarea value); its children never flow
                return
            _disp = node.style.get("display", "")
            if node is not self.node and (
                    node.tag in REPLACED_CONTROLS
                    or (_disp in ("inline-block", "inline-flex",
                                  "inline-table", "block", "flex",
                                  "grid", "table", "flow-root")
                        and node.tag not in ("img", "svg", "br"))):
                # a descendant inline-block (or a replaced form control)
                # becomes an atomic box; the container node itself (node is
                # self.node) falls through so its own children flow,
                # avoiding infinite re-entry.
                self.inline_block(node)
                return
            if node.tag == "br":
                self._forced_break = True
                self.new_line()
                self._forced_break = False
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
            lead, trail = inline_insets(node, self.width)
            if lead:
                self._inline_spacer(node, lead)
            for child in node.children:
                self.recurse(child)
            if trail:
                self._inline_spacer(node, trail)

    def _inline_spacer(self, node, width):
        """Open `width` px on the current line for an inline box's own
        margin/border/padding edge."""
        if not self.children:
            self.new_line()
        line = self.children[-1]
        prev = line.children[-1] if line.children else None
        line.children.append(InlineSpacer(node, width, line, prev))
        self.cursor_x += width

    def word(self, node, word, space_before=True):
        font = cached_font(node)
        w = measure(font, word)
        # trailing breaking spaces hang, so they are not part of what
        # has to fit — only what precedes them is
        fit = w if word[-1:] not in BREAK_SPACE \
            else measure(font, hang_trim(word))
        nowrap = white_space(node.style) in ("nowrap", "pre")
        # A run wider than the whole line can't fit however it wraps. Break
        # it between characters when the script allows: CJK/Hangul/Kana
        # always break between ideographs, and word-break:break-all /
        # overflow-wrap:break-word|anywhere break any long token (long
        # URLs, hashes). Otherwise it overflows as one word (as before).
        # A run of nothing but breaking spaces hangs: CSS Text 3 §5.2
        # keeps preserved trailing white space on the line it ends,
        # past the line box, rather than measuring it or carrying it
        # down. Only U+3000 and friends reach here — an ordinary space
        # was already collapsed away by then.
        if word and all(ch in BREAK_SPACE for ch in word):
            line = self.children[-1]
            prev = line.children[-1] if line.children else None
            line.children.append(
                TextLayout(node, word, line, prev, keep_spaces=True))
            self.cursor_x += w
            return
        # A CJK run breaks at the *last* opportunity that still fits, so
        # the test is the space left on this line, not the whole line —
        # otherwise a two-ideograph run that would fit on a fresh line
        # moves down whole and leaves a ragged hole behind it.
        crowded = (self.width > 0
                   and self.cursor_x + fit > self.width)
        if not nowrap and crowded:
            # only reached for a run that will not fit, so the ancestor
            # walks these three keywords need are cheap here
            wb = _inherited_kw(node, "word-break")
            ow = (_inherited_kw(node, "overflow-wrap")
                  or _inherited_kw(node, "word-wrap"))
            lb = _inherited_kw(node, "line-break")
            anywhere = wb == "break-all" or lb == "anywhere" \
                or ow == "anywhere"
            # break-word only breaks a run that will not fit even on a
            # line of its own; break-all and line-break:anywhere break
            # greedily. keep-all forbids the CJK opportunities outright,
            # but not the ones a space in the run provides.
            force = anywhere or (ow == "break-word" and fit > self.width)
            cjk = wb != "keep-all" and _has_cjk(word)
            if force or cjk or any(ch in BREAK_SPACE for ch in word):
                self._emit_broken(node, word, font,
                                  anywhere=force, cjk=cjk)
                return
        line = self.children[-1]
        # a leading space only when the source had whitespace here and we
        # are not at the start of a line. word-spacing is added to that
        # space, which is what makes it apply between words and not at
        # the edges of a line.
        sp = 0.0
        if space_before and line.children:
            advance = measure(font, " ")
            sp = advance + word_spacing_px(
                node, parse_px(node.style.get("font-size", "16px"), 16.0),
                advance)
        # a line may only break at a break OPPORTUNITY: source whitespace,
        # a CJK boundary (either side), or after a replaced atom. Without
        # one, adjacent tokens stick — naver's <strong>27.1</strong>°
        # temperature must not push its degree sign to the next line.
        can_break = space_before
        if not can_break and line.children:
            prev_atom = line.children[-1]
            prev_word = getattr(prev_atom, "word", None)
            if prev_word is None:            # image / inline-block atom
                can_break = True
            elif prev_word and _is_cjk(prev_word[-1]):
                can_break = True
        if not can_break and word and _is_cjk(word[0]):
            can_break = True
        if not nowrap and can_break and self.cursor_x + sp + fit > self.width \
                and self.cursor_x > 0:
            self.new_line()
            sp = 0.0
        line = self.children[-1]
        prev = line.children[-1] if line.children else None
        text = TextLayout(node, word, line, prev, keep_spaces=not space_before)
        line.children.append(text)
        self.cursor_x += sp + w

    def _emit_broken(self, node, word, font, anywhere=False, cjk=True):
        """Emit a run split at its break opportunities, wrapping across
        lines. `anywhere` breaks between any two characters (break-all,
        line-break: anywhere, an over-wide run under break-word);
        otherwise the opportunities are the ones the text itself gives.
        Segments carry no inter-segment space — CJK is written without
        them, and a breaking space stays inside its own segment."""
        segs = list(word) if anywhere else break_segments(word, cjk=cjk)
        for seg in segs:
            sw = measure(font, seg)
            sfit = sw if seg[-1:] not in BREAK_SPACE \
                else measure(font, hang_trim(seg))
            if self.cursor_x + sfit > self.width and self.cursor_x > 0:
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
        if w is not None and h is None:
            h = 0.0 if w == 0 else (w / ar if ar else (
                w * natural_h / natural_w if natural_w else w))
        elif h is not None and w is None:
            w = 0.0 if h == 0 else (h * ar if ar else (
                h * natural_w / natural_h if natural_h else h))
        elif w is None and h is None:
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

    def pre_wrapped(self, node, collapse, break_spaces=False):
        """white-space: pre-wrap / pre-line / break-spaces. Each source
        newline forces a break and long lines still wrap. pre-line
        collapses runs of whitespace (emit split words); pre-wrap
        preserves them (emit whitespace and word tokens verbatim,
        breaking between them).

        break-spaces differs from pre-wrap in what happens to a space
        run that reaches the edge: pre-wrap hangs it past the line box,
        break-spaces measures it like any other content and takes a
        break opportunity after every single preserved space, so the
        run itself splits across lines."""
        font = cached_font(node)
        anywhere = _inherited_kw(node, "line-break") == "anywhere" \
            or _inherited_kw(node, "word-break") == "break-all" \
            or _inherited_kw(node, "overflow-wrap") == "anywhere"
        keep_all = _inherited_kw(node, "word-break") == "keep-all"
        for i, raw in enumerate(node.text.split("\n")):
            if i > 0:
                self.new_line()
            if collapse:
                for word in css_words(raw):
                    self.word(node, word)
                continue
            for token in break_runs(raw):
                # a space run only has to be split apart when it is what
                # overflows; whole runs that fit stay one layout object
                blank = token[0] in BREAK_SPACE
                pieces = [token]
                crowded = (self.cursor_x + measure(font, token) > self.width)
                if anywhere and crowded and not blank:
                    pieces = list(token)
                elif break_spaces and blank and crowded:
                    pieces = list(token)
                elif not blank and crowded and not keep_all \
                        and _has_cjk(token):
                    # a run of ideographs breaks between characters, so
                    # it fills the line instead of overflowing it whole
                    pieces = break_segments(token)
                for piece in pieces:
                    w = measure(font, piece)
                    # a preserved space run hangs past the end of the
                    # line; break-spaces is the mode that says otherwise
                    wraps = not blank or break_spaces
                    if wraps and self.cursor_x + w > self.width \
                            and self.cursor_x > 0:
                        self.new_line()
                    line = self.children[-1]
                    prev = line.children[-1] if line.children else None
                    line.children.append(
                        TextLayout(node, piece, line, prev,
                                   keep_spaces=True))
                    self.cursor_x += w

    # ----- painting -----

    def paint(self):
        cmds = []
        if isinstance(self.node, Element):
            # opacity composites the whole subtree; the paint-relevant
            # case is "effectively invisible" (naver hides shell pieces
            # with opacity:0 until its app boots)
            if not paint_visible(self.node):
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
            side_widths = (self.bt, self.br, self.bb, self.bl)

            def border_color(side):
                raw = self.node.style.get(
                    f"border-{side}-color",
                    self.node.style.get("border-color", "#999999"))
                return safe_color(raw, default="")

            side_colors = tuple(border_color(side) for side in
                                ("top", "right", "bottom", "left"))
            uniform_border = (len(set(side_widths)) == 1
                              and len(set(side_colors)) == 1)

            # box-shadow: a flat offset rect behind the box (blur/spread
            # approximated away)
            shadow = box_shadow(self.node.style.get("box-shadow", ""))
            if shadow:
                dx, dy, scolor = shadow
                cmds.append(DrawRect(
                    x1 + dx, y1 + dy, x2 + dx, y2 + dy, scolor,
                    radius=radius))

            # a real gradient replaces the solid approximation; the
            # first-stop fallback stays for radial/conic/repeating and
            # for stop lists this parser cannot resolve
            grad = None
            for prop in ("background-image", "background"):
                grad = parse_linear_gradient(
                    self.node.style.get(prop, ""), x2 - x1, y2 - y1)
                if grad:
                    break

            # background-clip: the background's default box is the
            # border box, not the padding box — it paints *under* the
            # border, which is only invisible while the border is opaque
            clip = (self.node.style.get("background-clip")
                    or "").strip().casefold()
            if clip == "content-box":
                cx1, cy1 = self.x, self.y
                cx2, cy2 = self.x + self.width, self.y + self.height
            elif clip in ("padding-box", "text"):
                cx1, cy1, cx2, cy2 = x1, y1, x2, y2
            else:
                cx1, cy1 = x1 - self.bl, y1 - self.bt
                cx2, cy2 = x2 + self.br, y2 + self.bb

            def bg_fill(bx1, by1, bx2, by2, r):
                if grad:
                    return DrawGradient(bx1, by1, bx2, by2,
                                        grad[0], grad[1], radius=r)
                return DrawRect(bx1, by1, bx2, by2, color, radius=r)

            if radius > 0 and self.bt > 0 and uniform_border and color:
                # rounded box: border ring = outer rounded fill,
                # then the background inset by the border width
                b = self.bt
                cmds.append(DrawRect(x1 - b, y1 - b, x2 + b, y2 + b,
                                     side_colors[0], radius=radius + b))
                cmds.append(bg_fill(x1, y1, x2, y2, radius))
            else:
                if color:
                    cmds.append(bg_fill(cx1, cy1, cx2, cy2, radius))
                if self.bt > 0 and side_colors[0]:
                    cmds.append(DrawRect(
                        x1 - self.bl, y1 - self.bt, x2 + self.br, y1,
                        side_colors[0]))
                if self.bb > 0 and side_colors[2]:
                    cmds.append(DrawRect(
                        x1 - self.bl, y2, x2 + self.br, y2 + self.bb,
                        side_colors[2]))
                if self.bl > 0 and side_colors[3]:
                    cmds.append(DrawRect(
                        x1 - self.bl, y1, x1, y2, side_colors[3]))
                if self.br > 0 and side_colors[1]:
                    cmds.append(DrawRect(
                        x2, y1, x2 + self.br, y2, side_colors[1]))

            bg_img = paint_background_image(self.node, x1, y1, x2, y2)
            if bg_img:
                cmds.append(bg_img)

            # gap decorations sit above the container's own background
            # and below its items, the way a grid line would
            for rx1, ry1, rx2, ry2, rgb in getattr(self, "_gap_rules", ()):
                cmds.append(DrawRect(rx1, ry1, rx2, ry2, rgb))

            # outline: a ring outside the border box that takes up no
            # space. It is the focus indicator on every keyboard-driven
            # page, and it was drawing nothing at all.
            ow, ocolor, ooffset = outline_ring(
                self.node.style,
                parse_px(self.node.style.get("font-size", "16px"), 16.0))
            if ow > 0 and ocolor:
                o1x = x1 - self.bl - ooffset
                o1y = y1 - self.bt - ooffset
                o2x = x2 + self.br + ooffset
                o2y = y2 + self.bb + ooffset
                cmds.append(DrawRect(o1x - ow, o1y - ow, o2x + ow, o1y,
                                     ocolor))
                cmds.append(DrawRect(o1x - ow, o2y, o2x + ow, o2y + ow,
                                     ocolor))
                cmds.append(DrawRect(o1x - ow, o1y, o1x, o2y, ocolor))
                cmds.append(DrawRect(o2x, o1y, o2x + ow, o2y, ocolor))

            # an <iframe>'s box embeds its child document's painted
            # output (browser/frames.py), clipped to the frame rect
            frame = getattr(self.node, "_frame", None)
            if frame is not None:
                cmds.extend(frame.paint_cmds(
                    self.x, self.y, self.width, self.height))

            if self.node.tag in REPLACED_CONTROLS:
                cmds.extend(self._paint_control())

            if self.node.tag == "li" and _list_marker_visible(self.node):
                font = cached_font(self.node)
                color = safe_color(self.node.style.get("color", "black"))
                kind = list_style_type(self.node)
                cy = self.y + font.gg_linespace / 2
                if kind in ("disc", "circle", "square"):
                    r = 2
                    left = self.x - 12
                    if kind == "square":
                        cmds.append(DrawRect(left, cy - r,
                                             left + 2 * r, cy + r, color))
                    elif kind == "circle":
                        # hollow: the fill is the page behind it, which
                        # this rasterizer has no way to punch out, so the
                        # ring is drawn as a thin oval outline
                        cmds.append(DrawOval(left, cy - r,
                                             left + 2 * r, cy + r, color))
                        cmds.append(DrawOval(left + 1, cy - r + 1,
                                             left + 2 * r - 1, cy + r - 1,
                                             "#ffffff"))
                    else:
                        cmds.append(DrawOval(left, cy - r,
                                             left + 2 * r, cy + r, color))
                else:
                    label = marker_label(kind, _list_item_index(self.node))
                    if label:
                        w = measure(font, label)
                        cmds.append(DrawText(
                            self.x - 8 - w,
                            self.y + max(0.0, (font.gg_linespace
                                               - font.gg_linespace) / 2),
                            label, font, color))
            if self.node.tag == "hr":
                cmds.append(DrawLine(
                    self.x, self.y + 4, self.x + self.width, self.y + 4,
                    "#cccccc", 1))
                self.height = max(self.height, 9)

            # overflow:hidden clips descendants to the padding box
            if self._clips():
                cmds.append(DrawClipPush(x1, y1, x2, y2))
        return cmds

    # ----- form-control faces -----
    #
    # The engine draws its own widget faces: without them a checkbox is
    # an invisible 13px box (checked state entirely unpaintable) and a
    # <select> has no box at all. A page that styles a control itself —
    # the `appearance: none` idiom, which shows up as an author
    # background or border — keeps its own face; only the state
    # indicator (checkmark, radio dot, chevron, label) paints on top.

    _FACE_BG = "#ffffff"
    _FACE_BORDER = "#767676"
    _FACE_ACCENT = "#1a73e8"
    _BUTTON_BG = "#efefef"
    _PLACEHOLDER = "#9e9e9e"

    def _author_styled_face(self):
        """Whether the page supplies the control's own box appearance."""
        style = self.node.style
        if any((self.bt, self.br, self.bb, self.bl)):
            return True
        # Explicitly transparent backgrounds and zero/none borders are
        # author styling too: they intentionally remove the native face.
        # Checkboxes, radios and range sliders are the exception —
        # their native widget survives being given a background or a
        # border, which is why WPT's fallback references leave those
        # three alone while giving every other control `appearance:
        # none`.
        if self._input_type() not in ("checkbox", "radio", "range") \
                and any(disables_native_appearance(prop) for prop in style):
            return True
        for prop in ("appearance", "-webkit-appearance"):
            value = (style.get(prop) or "").strip().casefold()
            if value and value not in ("auto", "initial", "unset"):
                return True
        return False

    def _input_type(self):
        return self.node.attributes.get(
            "type", "text").strip().casefold()

    def _paint_control(self):
        tag = self.node.tag
        itype = self._input_type() if tag == "input" else ""
        if tag == "input" and itype in ("checkbox", "radio"):
            return self._paint_toggle(itype)
        if tag == "select":
            return self._paint_select()
        if tag == "textarea":
            return self._paint_textarea()
        if tag == "input" and itype in ("submit", "reset", "button"):
            return self._paint_button()
        return self._paint_text_field()

    def _face_rect(self, cmds, radius=2.0, bg=None):
        """Default box face (background + 1px border) when the page has
        not styled the control itself. The border is drawn INSIDE the
        box, so a control never paints outside its own layout rect.
        Returns True if it painted."""
        if self._author_styled_face():
            return False
        x2, y2 = self.x + self.width, self.y + self.height
        cmds.append(DrawRect(self.x, self.y, x2, y2,
                             self._FACE_BORDER, radius=radius))
        cmds.append(DrawRect(self.x + 1, self.y + 1, x2 - 1, y2 - 1,
                             bg or self._FACE_BG, radius=max(radius - 1, 0)))
        return True

    def _focus_ring(self, cmds, radius=2.0):
        """A 2px accent ring just outside the control (drawn first, so
        the face covers its inner edge) — the keyboard-focus affordance
        every native control has."""
        if not getattr(self.node, "is_focused", False):
            return
        x2, y2 = self.x + self.width, self.y + self.height
        cmds.append(DrawRect(self.x - 2, self.y - 2, x2 + 2, y2 + 2,
                             self._FACE_ACCENT, radius=radius + 2))

    def _paint_toggle(self, itype):
        """checkbox / radio: a real box or circle, and a visible checked
        state (previously `checked` painted nothing at all)."""
        cmds = []
        w, h = self.width, self.height
        x1, y1 = self.x, self.y
        x2, y2 = x1 + w, y1 + h
        checked = "checked" in self.node.attributes
        styled = self._author_styled_face()
        accent = safe_color(
            self.node.style.get("accent-color", ""), default="") \
            or self._FACE_ACCENT
        self._focus_ring(cmds, radius=w / 2 if itype == "radio" else 2.0)
        if itype == "radio":
            if not styled:
                cmds.append(DrawOval(x1, y1, x2, y2, self._FACE_BORDER))
                cmds.append(DrawOval(x1 + 1, y1 + 1, x2 - 1, y2 - 1,
                                     self._FACE_BG))
            if checked:
                # the dot: a filled circle inset to ~40% of the box
                inset = max(w * 0.28, 2.0)
                cmds.append(DrawOval(x1 + inset, y1 + inset,
                                     x2 - inset, y2 - inset, accent))
            return cmds
        # checkbox
        if not styled:
            edge = accent if checked else self._FACE_BORDER
            cmds.append(DrawRect(x1, y1, x2, y2, edge, radius=2.0))
            cmds.append(DrawRect(x1 + 1, y1 + 1, x2 - 1, y2 - 1,
                                 accent if checked else self._FACE_BG,
                                 radius=1.0))
        if checked:
            # a checkmark built from two strokes (no glyph font needed);
            # white on the accent fill, accent-coloured on an author face
            ink = self._FACE_BG if not styled else accent
            t = max(1.0, round(w / 8.0))
            cmds.append(DrawLine(x1 + w * 0.22, y1 + h * 0.52,
                                 x1 + w * 0.42, y1 + h * 0.72, ink, t))
            cmds.append(DrawLine(x1 + w * 0.42, y1 + h * 0.72,
                                 x1 + w * 0.78, y1 + h * 0.28, ink, t))
        return cmds

    def _selected_option_label(self):
        """The label a closed <select> shows: the selected option, else
        the first one (matching the submitted value, forms._select_values)."""
        options = [n for n in tree_to_list(self.node, [])
                   if isinstance(n, Element) and n.tag == "option"]
        chosen = next(
            (o for o in options if "selected" in o.attributes), None)
        if chosen is None:
            chosen = options[0] if options else None
        if chosen is None:
            return ""
        label = chosen.attributes.get("label")
        if label:
            return label.strip()
        # only this option's own text — malformed markup can still nest
        # options, and a nested one's label is not part of this label
        parts = []
        stack = list(chosen.children)
        while stack:
            n = stack.pop(0)
            if isinstance(n, Text):
                parts.append(n.text)
            elif isinstance(n, Element) and n.tag != "option":
                stack = list(n.children) + stack
        return " ".join("".join(parts).split())

    def _paint_select(self):
        cmds = []
        self._focus_ring(cmds)
        self._face_rect(cmds)
        font = cached_font(self.node)
        pad = 6.0
        chevron_w = 16.0
        text_w = max(self.width - pad - chevron_w, 0.0)
        label = self._selected_option_label()
        ty = self.y + max(0.0, (self.height - font.gg_linespace) / 2)
        if label and text_w > 4:
            label = _ellipsize(label, font, text_w)
            cmds.append(DrawText(self.x + pad, ty, label, font,
                                 safe_color(self.node.style.get("color"))))
        # dropdown chevron: two strokes forming a "v" on the right edge
        cx = self.x + self.width - chevron_w / 2 - 2
        cy = self.y + self.height / 2
        arm = 3.5
        cmds.append(DrawLine(cx - arm, cy - arm / 2, cx, cy + arm / 2,
                             "#5f6368", 2))
        cmds.append(DrawLine(cx, cy + arm / 2, cx + arm, cy - arm / 2,
                             "#5f6368", 2))
        return cmds

    def _textarea_value(self):
        """Current textarea contents: the typed value when the shells
        have recorded one, else the original child text."""
        if "value" in self.node.attributes:
            return self.node.attributes["value"]
        return "".join(t.text for t in tree_to_list(self.node, [])
                       if isinstance(t, Text))

    def _paint_textarea(self):
        cmds = []
        self._focus_ring(cmds)
        self._face_rect(cmds)
        font = cached_font(self.node)
        value = self._textarea_value()
        pad = 3.0
        line_h = font.gg_linespace
        avail_w = max(self.width - 2 * pad, 0.0)
        color = safe_color(self.node.style.get("color"))
        # clip so long content cannot escape the control's own box
        cmds.append(DrawClipPush(self.x, self.y,
                                 self.x + self.width,
                                 self.y + self.height))
        ty = self.y + pad
        last_x = self.x + pad
        for raw in (value.split("\n") if value else []):
            if ty > self.y + self.height:
                break
            shown = _ellipsize(raw, font, avail_w) if raw else ""
            if shown:
                cmds.append(DrawText(self.x + pad, ty, shown, font, color))
            last_x = self.x + pad + measure(font, shown)
            ty += line_h
        if getattr(self.node, "is_focused", False):
            cy = min(ty - line_h, self.y + self.height - line_h) \
                if value else self.y + pad
            cmds.append(DrawLine(last_x, cy, last_x, cy + line_h,
                                 "#333333", 1))
        cmds.append(DrawClipPop())
        return cmds

    def _paint_button(self):
        cmds = []
        self._focus_ring(cmds)
        self._face_rect(cmds, bg=self._BUTTON_BG)
        font = cached_font(self.node)
        label = self.node.attributes.get("value") or {
            "submit": "제출", "reset": "재설정"}.get(self._input_type(), "")
        if label:
            tw = measure(font, label)
            tx = self.x + max(0.0, (self.width - tw) / 2)
            ty = self.y + max(0.0, (self.height - font.gg_linespace) / 2)
            cmds.append(DrawText(tx, ty, label, font,
                                 safe_color(self.node.style.get("color"))))
        return cmds

    def _paint_text_field(self):
        """text/search/password/... inputs: value or placeholder, clipped
        to the control, with the caret when focused."""
        cmds = []
        self._focus_ring(cmds)
        self._face_rect(cmds)
        node = self.node
        raw = node.attributes.get("value", "")
        itype = self._input_type()
        if itype == "password" and raw:
            raw = "•" * len(raw)
        if itype == "file":
            raw = raw or "파일 선택"
        text = raw or node.attributes.get("placeholder", "")
        font = cached_font(node)
        ty = self.y + max(0.0, (self.height - font.metrics("linespace")) / 2)
        cmds.append(DrawClipPush(self.x, self.y,
                                 self.x + self.width,
                                 self.y + self.height))
        if text.strip():
            tcolor = (safe_color(node.style.get("color")) if raw
                      else self._PLACEHOLDER)
            cmds.append(DrawText(self.x, ty, text, font, tcolor))
        if getattr(node, "is_focused", False):
            # caret after the typed value (not the placeholder)
            cx = self.x + (measure(font, raw) if raw else 0)
            ch = font.metrics("linespace")
            cmds.append(DrawLine(cx, ty, cx, ty + ch, "#333333", 1))
        cmds.append(DrawClipPop())
        return cmds

    def _clips(self):
        if not isinstance(self.node, Element):
            return False
        for axis in ("overflow", "overflow-x", "overflow-y"):
            if self.node.style.get(axis) in ("hidden", "clip", "scroll"):
                return True
        # `overflow:auto` clips only once the box is a real scroller
        # (definitely sized, so its content can actually overflow and
        # the user can scroll to it). An auto box with content-driven
        # height simply grows, and clipping that would hide readable
        # content — the reason auto went unclipped before scrolling
        # existed.
        return any(scroll_axes(self.node))

    def paint_after(self):
        cmds = [DrawClipPop()] if self._clips() else []
        # the scrollbar paints after (on top of) the scrolled content
        # and outside the clip, so it always stays visible
        cmds.extend(self._scrollbar_cmds())
        return cmds

    _SCROLLBAR_W = 6.0
    _SCROLLBAR_MIN = 20.0
    _SCROLLBAR_INK = "#b0b0b0"

    def _scrollbar_cmds(self):
        """A thin indicator inside an overflowing scroll container —
        what makes an inner scroll area discoverable at all."""
        if not is_scroll_container(self):
            return []
        max_y, max_x = scroll_range(self)
        y, x = scroll_position(self)
        cmds = []
        w, h = self.width, self.height
        bar = self._SCROLLBAR_W
        if max_y > 0 and h > self._SCROLLBAR_MIN:
            content = h + max_y
            thumb = max(h * h / content, self._SCROLLBAR_MIN)
            top = self.y + (y / max_y) * (h - thumb)
            cmds.append(DrawRect(
                self.x + w - bar, top, self.x + w, top + thumb,
                self._SCROLLBAR_INK, radius=bar / 2))
        if max_x > 0 and w > self._SCROLLBAR_MIN:
            content = w + max_x
            thumb = max(w * w / content, self._SCROLLBAR_MIN)
            left = self.x + (x / max_x) * (w - thumb)
            cmds.append(DrawRect(
                left, self.y + h - bar, left + thumb, self.y + h,
                self._SCROLLBAR_INK, radius=bar / 2))
        return cmds


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
        else:
            edge = sum(
                parse_size(node.style.get(p), avail, em) or 0.0
                for p in ("padding-left", "padding-right"))
            edge += _style_border_width(
                node.style, "left", avail, em)
            edge += _style_border_width(
                node.style, "right", avail, em)
            if node.style.get(
                    "box-sizing", "content-box").strip().casefold() \
                    == "border-box":
                w = max(w, edge)
            else:
                w += edge
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
            self.x = self.parent.x + getattr(self.parent, "indent", 0.0)

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
        self.indent = 0.0

    def _is_last_line(self):
        """Is this the final line of its block? text-align-last styles
        it, and `justify` deliberately does not stretch it — a justified
        paragraph whose last line were stretched too would have three
        words spread across the full column."""
        lines = getattr(self.parent, "children", None)
        return not lines or lines[-1] is self

    def _justify(self, free):
        """Spread `free` px across the gaps between the line's atoms.

        The gaps are the word boundaries, so a line of one word has none
        and stays where it is. Each atom moves by the total inserted to
        its left, which keeps the words themselves unstretched."""
        gaps = len(self.children) - 1
        if gaps < 1:
            return
        step = free / gaps
        for i, word in enumerate(self.children):
            word.x += step * i

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
            # 'normal': the 1.25 leading belongs to the text strut. An
            # atomic box (image, inline-block) brings its own height and
            # must not be inflated by it — naver's 58px search input sat
            # alone in a line box and came out 58 × 1.25 = 72.5px, which
            # pushed the search bar down over the shortcut row below it.
            # `natural` is a floor so a mixed line never loses the room
            # its text descenders already claimed.
            text_natural = max(
                (ascent(c) + descent(c)
                 for c in self.children if c.font is not None),
                default=0.0)
            atomic_max = max(
                (c.height for c in self.children if c.font is None),
                default=0.0)
            line = max(text_natural * 1.25, atomic_max, natural)
            factor = line / natural if natural > 0 else 1.25
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
        try:
            measuring = self.parent._document()._measuring
        except Exception:
            measuring = False
        align = resolved_text_align(self.node, self._is_last_line())
        if not measuring and self.children and align != "left":
            last = self.children[-1]
            used = (last.x + last.width) - self.x
            free = self.width - used
            if free > 0:
                if align == "justify":
                    self._justify(free)
                else:
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
        def _has_own_box(el):
            # block-level and atomic ancestors paint their own
            # background in their own box — re-filling it at word
            # granularity painted naver's white card background OVER
            # the 로그인 label inside the green login button (the walk
            # ran past the button up to the card because the line's
            # block node was a bare Text)
            if _child_is_block_level(el):
                return True
            return el.style.get("display", "") in (
                "inline-block", "inline-flex", "inline-table")

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
            while isinstance(anc, Element) and anc is not block \
                    and not _has_own_box(anc):
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
            if not paint_visible(el):
                continue
            color = safe_color(el.style.get("background-color"), default="")
            if color and x1 > x0:
                cmds.append(DrawRect(x0, self.y, x1, self.y + self.height,
                                     color))
        return cmds


class InlineSpacer:
    """Zero-height horizontal space on a line.

    An inline box's own margin, border and padding open a gap before its
    first atom and after its last one, but the inline box has no layout
    object of its own here — only its text does. This is that gap, so it
    participates in line breaking and in text-align like any other atom
    while drawing nothing.
    """

    __slots__ = ("node", "parent", "previous", "children", "x", "y",
                 "width", "height", "margin_top", "margin_bottom",
                 "font", "word", "keep_spaces")

    def __init__(self, node, width, parent, previous):
        self.node = node
        self.parent = parent
        self.previous = previous
        self.children = []
        self.x = 0
        self.y = 0
        self.width = width
        self.height = 0
        self.margin_top = 0
        self.margin_bottom = 0
        self.font = None
        self.word = ""
        self.keep_spaces = True

    def layout(self):
        if self.previous:
            self.x = self.previous.x + self.previous.width
        else:
            self.x = self.parent.x + getattr(self.parent, "indent", 0.0)

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
            advance = measure(prev_font, " ")
            space = 0 if self.keep_spaces else (
                advance + word_spacing_px(
                    self.node, self.font.size, advance))
            self.x = self.previous.x + self.previous.width + space
        else:
            self.x = self.parent.x + getattr(self.parent, "indent", 0.0)
        shown = transformed_text(self.node, self.word)
        self.spacing = letter_spacing_px(self.node, self.font.size)
        self.width = measure(self.font, shown)
        if self.spacing:
            # one advance per character, including after the last —
            # which is what every engine does and what makes a
            # letter-spaced word wider than its glyphs
            self.width += self.spacing * len(shown)
        self.height = self.font.gg_linespace

    def paint(self):
        if not paint_visible(self.node):
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
                and white_space(self.node.style) == "nowrap":
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
        word = transformed_text(self.node, word)
        spacing = getattr(self, "spacing", 0.0)
        if spacing:
            # spread the glyphs: one DrawText per character, since the
            # text primitive has no spacing of its own
            cmds = []
            x = self.x
            for ch in word:
                cmds.append(DrawText(x, self.y, ch, self.font, color))
                x += measure(self.font, ch) + spacing
        else:
            cmds = [DrawText(self.x, self.y, word, self.font, color)]
        # text-decoration is not an inherited property — it *propagates*
        # to in-flow descendants, which is a different rule. Reading it
        # off the text node found nothing, so nothing was ever
        # underlined: not `text-decoration: underline`, and not `<a>`
        # either, whose underline comes from the UA sheet on the
        # element rather than on the text inside it.
        decoration = "none"
        anc = self.node
        while anc is not None:
            if isinstance(anc, Element):
                got = anc.style.get("text-decoration", "")
                if got and got != "none":
                    decoration = got
                    break
                # a block container ends the propagation
                if anc.style.get("display", "inline") not in (
                        "inline", "inline-block", "list-item"):
                    break
            anc = anc.parent
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
            self.x = self.parent.x + getattr(self.parent, "indent", 0.0)

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
        if not paint_visible(self.node):
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
        return float(value) if value else None
    except ValueError:
        return None


def _z_index(obj):
    """Stacking level of a layout subtree for z-index ordering. Only
    positioned boxes carry one; z-index:auto inherits the nearest
    positioned ancestor's level (the box belongs to that ancestor's
    stacking context — the abs pass flattens nested absolutes into
    siblings, and without inheritance a button's own ::before icon
    sorted BELOW the z-indexed button and vanished under its fill)."""
    node = getattr(obj, "node", None)
    if not isinstance(node, Element):
        return 0
    if node.style.get("position", "static") == "static":
        # A grid or flex item paints in z-index order whether or not it
        # is positioned (CSS Grid 1 §6.1, Flexbox §5.4) — two items
        # sharing one grid area is exactly the case that needs it.
        parent = getattr(node, "parent", None)
        if isinstance(parent, Element) \
                and layout_mode(parent) in ("grid", "flex"):
            try:
                return int(node.style.get("z-index", "auto"))
            except (ValueError, TypeError):
                return 0
        return 0  # z-index has no effect on other static boxes
    cur = node
    for _ in range(32):
        if not isinstance(cur, Element):
            break
        if cur.style.get("position", "static") != "static":
            try:
                return int(cur.style.get("z-index", "auto"))
            except (ValueError, TypeError):
                pass  # auto: join the parent stacking context
        cur = getattr(cur, "parent", None)
    return 0


def parse_transform(value, w, h):
    """CSS transform -> (dx, dy, sx, sy, hidden).

    Translation and axis-aligned scale are honoured; percentages in a
    translate resolve against the element's own border box, which is
    the spec's reference box for it. A zero scale hides the subtree.

    Rotation and skew are still ignored, and that is a real gap rather
    than a shrug: they turn a rect into a quad, which this display
    list has no command for and this rasterizer no polygon fill.
    """
    dx = dy = 0.0
    sx = sy = 1.0
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
            a, b = px(args[0], 1), px(args[1], 1)
            c, d = px(args[2], 1), px(args[3], 1)
            if a == 0 and d == 0:
                hidden = True
            elif b == 0 and c == 0:      # no rotation or skew in it
                sx *= a
                sy *= d
        elif fn in ("scale", "scale3d"):
            fx = px(args[0], 1)
            fy = px(args[1], 1) if len(args) > 1 else fx
            if fx == 0 or fy == 0:
                hidden = True
            else:
                sx *= fx
                sy *= fy
        elif fn == "scalex":
            fx = px(args[0], 1)
            hidden = hidden or fx == 0
            sx *= fx or 1.0
        elif fn == "scaley":
            fy = px(args[0], 1)
            hidden = hidden or fy == 0
            sy *= fy or 1.0
    return dx, dy, sx, sy, hidden


_ORIGIN_KEYWORDS = {"left": 0.0, "top": 0.0, "center": 0.5,
                    "right": 1.0, "bottom": 1.0}


def _transform_origin(box, w, h):
    """Absolute (x, y) a transform is applied about. Defaults to the
    centre of the border box, which is what `50% 50%` means."""
    spec = (getattr(box.node, "style", None) or {}).get(
        "transform-origin", "")
    parts = [p.strip().casefold() for p in spec.split()[:2]]
    fx = fy = None
    # `top left` and `left top` mean the same thing, so a side keyword
    # names its own axis rather than the position it was written in
    rest = []
    for part in parts:
        if part in ("left", "right"):
            fx = _ORIGIN_KEYWORDS[part]
        elif part in ("top", "bottom"):
            fy = _ORIGIN_KEYWORDS[part]
        else:
            rest.append(part)
    for part in rest:
        axis_x = fx is None
        if part == "center":
            value = 0.5
        else:
            val = parse_size(part, w if axis_x else h)
            base = w if axis_x else h
            if val is None or not base:
                continue
            value = val / base
        if axis_x:
            fx = value
        elif fy is None:
            fy = value
    fx = 0.5 if fx is None else fx
    fy = 0.5 if fy is None else fy
    left = box.x - box.pl - box.bl
    top = box.y - box.pt - box.bt
    return left + w * fx, top + h * fy


_FILTER_OPACITY_RE = re.compile(
    r"opacity\(\s*([0-9.]+)(%?)\s*\)", re.I)


def own_opacity(node):
    """The element's own `opacity`, or None when it is fully opaque.

    `filter: opacity()` composes with it: the filter function is
    defined as doing what the property does, so the two multiply, and
    supporting it here costs nothing now that the property works.
    Every other filter function needs a per-pixel colour transform
    this rasterizer has no command for and is still ignored.
    """
    style = getattr(node, "style", None) or {}
    value = 1.0
    raw = style.get("opacity")
    if raw is not None:
        try:
            value *= max(float(str(raw).strip()), 0.0)
        except (TypeError, ValueError):
            pass
    m = _FILTER_OPACITY_RE.search(style.get("filter") or "")
    if m:
        try:
            found = float(m.group(1))
            value *= max(found / 100.0 if m.group(2) else found, 0.0)
        except ValueError:
            pass
    return None if value >= 1.0 else value


_INSET_RE = re.compile(r"inset\(([^)]*)\)", re.I)


def clip_path_inset(node, em, x1, y1, x2, y2):
    """The rect a rectangular `clip-path` clips to, or None.

    Only `inset()` is handled, and it is handled exactly: it is a
    rectangle, which is the one shape this display list has a clip
    command for. circle(), ellipse() and polygon() need a real shape
    clip and are still ignored rather than approximated by their
    bounding box — a box is not a circle, and drawing one where the
    author asked for the other is worse than drawing nothing special.
    """
    spec = (getattr(node, "style", None) or {}).get("clip-path") or ""
    m = _INSET_RE.search(spec)
    if not m:
        return None
    parts = m.group(1).split("round")[0].split()
    if not parts:
        return None
    w, h = x2 - x1, y2 - y1
    sides = []
    for i in range(4):
        # top right bottom left, filled in the CSS shorthand way
        token = parts[i] if i < len(parts) else \
            parts[i - 2] if i >= 2 and len(parts) > i - 2 else \
            parts[1] if i == 3 and len(parts) > 1 else parts[0]
        value = parse_size(token, h if i % 2 == 0 else w, em)
        sides.append(0.0 if value is None else value)
    top, right, bottom, left = sides
    return (x1 + left, y1 + top, max(x2 - right, x1 + left),
            max(y2 - bottom, y1 + top))


def paint_tree(layout_object, display_list):
    # opacity fades the whole subtree, so it brackets everything the box
    # paints rather than tinting one command
    if not isinstance(layout_object, BlockLayout):
        return _paint_tree_uncomposited(layout_object, display_list)
    alpha = own_opacity(layout_object.node)
    box = layout_object
    clip = clip_path_inset(
        box.node,
        parse_px((getattr(box.node, "style", None) or {}).get(
            "font-size", "16px"), 16.0),
        box.x - box.pl - box.bl, box.y - box.pt - box.bt,
        box.x + box.width + box.pr + box.br,
        box.y + box.height + box.pb + box.bb)
    if alpha is None and clip is None:
        return _paint_tree_uncomposited(layout_object, display_list)
    inner = _paint_tree_uncomposited(layout_object, [])
    if not inner:
        return display_list
    if clip is not None:
        inner = [DrawClipPush(*clip)] + inner + [DrawClipPop()]
    if alpha is not None:
        inner = [DrawOpacityPush(alpha)] + inner + [DrawOpacityPop()]
    display_list.extend(inner)
    return display_list


def _paint_tree_uncomposited(layout_object, display_list):
    # transform applies to an element's principal box (blocks only —
    # line/text boxes share their block's node and must not re-apply)
    if isinstance(layout_object, BlockLayout):
        style = getattr(layout_object.node, "style", None)
        tf = style.get("transform") if style else None
        sticky = _sticky_metrics(layout_object)
        inherited_tfs = getattr(layout_object, "_abs_transforms", ()) or ()
        # absolutely-positioned boxes paint from the document's abs
        # pass, outside their ancestors' clip brackets — re-apply the
        # overflow clips recorded at queue time so they can't escape
        aclips = getattr(layout_object, "_abs_clips", None) or []
        if (tf and tf != "none") or inherited_tfs \
                or sticky is not None or aclips:
            sub = _paint_tree_inner(layout_object, [])
            # Absolutely-positioned descendants are painted from the
            # document pass, outside their original ancestor subtree.
            # Replay the translations of those visual ancestors here.
            for ancestor in inherited_tfs:
                ast = getattr(ancestor.node, "style", None)
                atf = ast.get("transform") if ast else None
                if not atf or atf == "none":
                    continue
                aw = (ancestor.bl + ancestor.pl + ancestor.width
                      + ancestor.pr + ancestor.br)
                ah = (ancestor.bt + ancestor.pt + ancestor.height
                      + ancestor.pb + ancestor.bb)
                dx, dy, sx, sy, hidden = parse_transform(atf, aw, ah)
                if hidden:
                    return display_list
                if sx != 1.0 or sy != 1.0:
                    ox, oy = _transform_origin(ancestor, aw, ah)
                    scale_cmds_about(sub, sx, sy, ox, oy)
                if dx or dy:
                    translate_cmds(sub, dx, dy)
            if tf and tf != "none":
                bw = (layout_object.bl + layout_object.pl
                      + layout_object.width + layout_object.pr
                      + layout_object.br)
                bh = (layout_object.bt + layout_object.pt
                      + layout_object.height + layout_object.pb
                      + layout_object.bb)
                dx, dy, sx, sy, hidden = parse_transform(tf, bw, bh)
                if hidden:
                    return display_list
                if sx != 1.0 or sy != 1.0:
                    ox, oy = _transform_origin(layout_object, bw, bh)
                    scale_cmds_about(sub, sx, sy, ox, oy)
                if dx or dy:
                    translate_cmds(sub, dx, dy)
            if aclips:
                pre = []
                for cbox in aclips:
                    pre.append(DrawClipPush(
                        cbox.x - cbox.pl, cbox.y - cbox.pt,
                        cbox.x + cbox.width + cbox.pr,
                        cbox.y + cbox.height + cbox.pb))
                sub = pre + sub + [DrawClipPop() for _ in aclips]
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
    # a scroll container offsets its DESCENDANTS (never its own
    # background/border/scrollbar, which stay put) — the same shape as
    # an iframe painting its child document at -scroll
    sdy, sdx = (0.0, 0.0)
    if is_scroll_container(layout_object):
        sdy, sdx = scroll_position(layout_object)
    for i, child in enumerate(layout_object.children):
        sub = []
        paint_tree(child, sub)
        if sdy or sdx:
            translate_cmds(sub, -sdx, -sdy)
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
