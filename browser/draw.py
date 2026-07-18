"""Display list: paint commands.

Each command can execute onto a tkinter canvas (fallback path) or
serialize itself into a flat tuple for the native Rust rasterizer:
(kind, x1, y1, x2, y2, rgb, aux, font_id, text)
kind: 0=rect(aux=corner radius) 1=text(aux=font size)
2=line(aux=thickness) 3=oval 4=image 5=background image
(font_id=image id, text="off_x off_y tile_w tile_h rep_x rep_y")
6=clip push (x1,y1,x2,y2 = clip rect) 7=clip pop
"""

import re

from .colors import to_rgb

_URL_RE = re.compile(r"url\(\s*['\"]?([^'\")]+)['\"]?\s*\)", re.I)
_POS_KEYWORDS = {"left", "right", "top", "bottom", "center"}


def parse_background(style):
    """First background layer of a computed style, or None.
    Returns {url, position: [tok, tok], size: [tok, ...], repeat}.
    Longhand properties win; missing pieces fall back to tokens of the
    `background` shorthand. Gradients (no url) return None."""
    shorthand = style.get("background", "").split(",")[0]
    image = style.get("background-image", "").split(",")[0]
    m = _URL_RE.search(image) or _URL_RE.search(shorthand)
    if not m:
        return None
    url = m.group(1).strip()

    position = style.get("background-position", "")
    size = style.get("background-size", "")
    repeat = style.get("background-repeat", "")
    # mine the shorthand for whatever longhands didn't specify:
    # "url(x) no-repeat -10px -20px" / "url(x) center/cover"
    rest = _URL_RE.sub(" ", shorthand)
    rest = re.sub(r"(?:rgba?|var|linear-gradient)\([^)]*\)", " ", rest)
    if "/" in rest and not size:
        rest, size = rest.split("/", 1)
    tokens = rest.replace(",", " ").split()
    pos_tokens = []
    for t in tokens:
        low = t.casefold()
        if not repeat and low in ("repeat", "no-repeat",
                                  "repeat-x", "repeat-y"):
            repeat = low
        elif low in _POS_KEYWORDS or low[:1].isdigit() \
                or low.startswith(("-", ".")):
            pos_tokens.append(low)
    if not position and pos_tokens:
        position = " ".join(pos_tokens[:2])
    return {
        "url": url,
        "position": position.split()[:2],
        "size": size.split()[:2],
        "repeat": repeat or "repeat",
    }


def scale_cmds(cmds, scale):
    """Scale native cmd tuples from CSS px to device px (HiDPI).
    aux is a font size for text (1), a thickness for lines (2), and a
    corner radius for rects (0); image cmds (4) carry width/height in
    x2/y2, so all four coordinates scale for every kind. Background
    image cmds (5) carry offsets/tile size in the text field."""
    if scale == 1.0:
        return cmds
    out = []
    for kind, x1, y1, x2, y2, rgb, aux, font_id, text in cmds:
        if kind in (0, 1, 2):
            aux *= scale
        if kind == 5:
            p = text.split()
            text = " ".join(f"{float(v) * scale:.2f}" for v in p[:4]) \
                + " " + " ".join(p[4:])
        out.append((kind, x1 * scale, y1 * scale, x2 * scale,
                    y2 * scale, rgb, aux, font_id, text))
    return out


def translate_cmds(cmds, dx, dy):
    """Shift painted commands in place (CSS transform: translate is
    paint-only — sibling layout is unaffected). Every command class
    keeps its geometry in left/top/right/bottom, so one shift covers
    text, rects, images, background tiles, and clip brackets alike."""
    for c in cmds:
        c.left += dx
        c.top += dy
        if hasattr(c, "right"):
            c.right += dx
        c.bottom += dy
    return cmds


class DrawText:
    def __init__(self, x, y, text, font, color):
        self.left = x
        self.top = y
        self.text = text
        self.font = font
        self.color = color
        linespace = getattr(font, "gg_linespace", None)
        if linespace is None:
            linespace = font.metrics("linespace")
        self.bottom = y + linespace

    def execute(self, scroll, canvas):
        canvas.create_text(
            self.left, self.top - scroll,
            text=self.text, font=self.font,
            fill=self.color, anchor="nw",
        )

    def native(self, scroll, hscroll=0.0):
        return (1, self.left - hscroll, self.top - scroll, 0.0, 0.0,
                to_rgb(self.color), float(self.font.size),
                self.font.id, self.text)


class DrawRect:
    def __init__(self, x1, y1, x2, y2, color, radius=0.0):
        self.left = x1
        self.top = y1
        self.right = x2
        self.bottom = y2
        self.color = color
        self.radius = radius

    def execute(self, scroll, canvas):
        # tk fallback draws square corners (no rounded-rect primitive)
        canvas.create_rectangle(
            self.left, self.top - scroll,
            self.right, self.bottom - scroll,
            width=0, fill=self.color,
        )

    def native(self, scroll, hscroll=0.0):
        return (0, self.left - hscroll, self.top - scroll,
                self.right - hscroll, self.bottom - scroll,
                to_rgb(self.color), float(self.radius), 0, "")


class DrawLine:
    def __init__(self, x1, y1, x2, y2, color, thickness=1):
        self.left = x1
        self.top = y1
        self.right = x2
        self.bottom = y2
        self.color = color
        self.thickness = thickness

    def execute(self, scroll, canvas):
        canvas.create_line(
            self.left, self.top - scroll,
            self.right, self.bottom - scroll,
            fill=self.color, width=self.thickness,
        )

    def native(self, scroll, hscroll=0.0):
        return (2, self.left - hscroll, self.top - scroll,
                self.right - hscroll, self.bottom - scroll,
                to_rgb(self.color), float(self.thickness), 0, "")


class DrawImage:
    def __init__(self, x, y, width, height, image_id):
        self.left = x
        self.top = y
        self.right = x + width
        self.bottom = y + height
        self.width = width
        self.height = height
        self.image_id = image_id

    def execute(self, scroll, canvas):
        # tk fallback path has no decoded pixels: placeholder
        canvas.create_rectangle(
            self.left, self.top - scroll,
            self.right, self.bottom - scroll,
            width=1, outline="#bbbbbb", fill="#eeeeee",
        )

    def native(self, scroll, hscroll=0.0):
        return (4, self.left - hscroll, self.top - scroll,
                self.width, self.height,
                (0, 0, 0), 0.0, self.image_id, "")


class DrawBgImage:
    """One background layer: image tiled/placed inside a box."""

    def __init__(self, x, y, w, h, image_id,
                 off_x, off_y, tile_w, tile_h, rep_x, rep_y):
        self.left = x
        self.top = y
        self.right = x + w
        self.bottom = y + h
        self.image_id = image_id
        self.off_x = off_x
        self.off_y = off_y
        self.tile_w = tile_w
        self.tile_h = tile_h
        self.rep_x = rep_x
        self.rep_y = rep_y

    def execute(self, scroll, canvas):
        pass  # tk fallback path has no decoded pixels

    def native(self, scroll, hscroll=0.0):
        params = (f"{self.off_x:.2f} {self.off_y:.2f} "
                  f"{self.tile_w:.2f} {self.tile_h:.2f} "
                  f"{1 if self.rep_x else 0} {1 if self.rep_y else 0}")
        return (5, self.left - hscroll, self.top - scroll,
                self.right - self.left, self.bottom - self.top,
                (0, 0, 0), 0.0, self.image_id, params)


class DrawClipPush:
    """Intersect the clip region with this rect until the matching pop.
    Always survives viewport culling (top/bottom span the page) so the
    clip stack stays balanced."""

    def __init__(self, x1, y1, x2, y2):
        self.left = x1
        self.right = x2
        self.clip_top = y1
        self.clip_bottom = y2
        self.top = -1e9
        self.bottom = 1e9

    def execute(self, scroll, canvas):
        pass  # tk fallback does not clip

    def native(self, scroll, hscroll=0.0):
        return (6, self.left - hscroll, self.clip_top - scroll,
                self.right - hscroll, self.clip_bottom - scroll,
                (0, 0, 0), 0.0, 0, "")


class DrawClipPop:
    def __init__(self):
        self.left = self.right = 0
        self.top = -1e9
        self.bottom = 1e9

    def execute(self, scroll, canvas):
        pass

    def native(self, scroll, hscroll=0.0):
        return (7, 0.0, 0.0, 0.0, 0.0, (0, 0, 0), 0.0, 0, "")


class DrawOval:
    def __init__(self, x1, y1, x2, y2, color):
        self.left = x1
        self.top = y1
        self.right = x2
        self.bottom = y2
        self.color = color

    def execute(self, scroll, canvas):
        canvas.create_oval(
            self.left, self.top - scroll,
            self.right, self.bottom - scroll,
            width=0, fill=self.color,
        )

    def native(self, scroll, hscroll=0.0):
        return (3, self.left - hscroll, self.top - scroll,
                self.right - hscroll, self.bottom - scroll,
                to_rgb(self.color), 0.0, 0, "")
