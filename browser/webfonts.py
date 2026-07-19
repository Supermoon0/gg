"""@font-face: extract descriptors from CSS text and register the
fetched fonts with the native text engine. TTF/OTF only (fontdue has
woff2 decompresses to ttf in Rust) — plain woff is still skipped."""

import re

from . import textengine

_FACE_RE = re.compile(r"@font-face\s*\{([^}]*)\}", re.IGNORECASE)
_DECL_RE = {
    "family": re.compile(r"font-family\s*:\s*([^;}]+)", re.IGNORECASE),
    # src runs to the end of the face body: data: URLs carry ';'
    # (";base64,") inside url(), so stopping at ';' would truncate
    "src": re.compile(r"src\s*:\s*([^}]+)", re.IGNORECASE),
    "weight": re.compile(r"font-weight\s*:\s*([^;}]+)", re.IGNORECASE),
    "style": re.compile(r"font-style\s*:\s*([^;}]+)", re.IGNORECASE),
}
_URL_RE = re.compile(r"url\(\s*['\"]?([^'\")]+)['\"]?\s*\)",
                     re.IGNORECASE)
# sources fontdue can actually parse, most preferred first
_LOADABLE = (".ttf", ".otf", ".woff2")


def parse_font_faces(css_texts):
    """[(family, bold, italic, url)] for every loadable @font-face."""
    faces = []
    for css in css_texts:
        for m in _FACE_RE.finditer(css):
            body = m.group(1)
            fam = _DECL_RE["family"].search(body)
            src = _DECL_RE["src"].search(body)
            if not fam or not src:
                continue
            family = fam.group(1).strip().strip("'\"").casefold()
            if not family:
                continue
            urls = _URL_RE.findall(src.group(1))
            if not urls:
                continue
            url = next(
                (u for u in urls
                 if u.split("?")[0].casefold().endswith(_LOADABLE)
                 or u.startswith("data:")),
                None)
            if url is None:
                continue  # woff(1)-only face: skip
            bold = False
            wm = _DECL_RE["weight"].search(body)
            if wm:
                w = wm.group(1).strip().casefold()
                try:
                    bold = int(w) >= 600
                except ValueError:
                    bold = w == "bold"
            sm = _DECL_RE["style"].search(body)
            italic = bool(sm) and "italic" in sm.group(1).casefold()
            faces.append((family, bold, italic, url))
    return faces


def load_web_fonts(css_texts, fetch):
    """Fetch + register each loadable @font-face; fetch(url) -> bytes
    (resolution against the page URL is the caller's). Returns the
    number of fonts actually registered."""
    if not textengine.available():
        return 0
    loaded = 0
    for family, bold, italic, url in parse_font_faces(css_texts):
        try:
            data = fetch(url)
        except Exception:
            continue
        if data and textengine.engine().load_font(
                family, bold, italic, bytes(data)):
            loaded += 1
    return loaded
