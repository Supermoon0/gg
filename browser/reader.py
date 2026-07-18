# -*- coding: utf-8 -*-
"""Reader mode: surface content that ships as inline JSON data.

App-shell pages (naver.com and friends) send their content as inline
``window["EAGER-DATA"][key] = {...}`` JSON and draw it with a JS app
the engine cannot fully boot yet. This module mines those blocks for
(title, url) pairs and injects a readable section into the HTML source
*before parsing*, so both style engines and the layout pipeline treat
it as ordinary markup.
"""

import json
import re

# EAGER-DATA keys that are ads/plumbing, never content
_SKIP_KEY_PARTS = ("AD", "SSP", "GV", "BANNER", "EVENT")

# friendlier section titles for known keys
_TITLES = {
    "PC-NEWSSTAND-YONHAP": "주요 뉴스 (연합뉴스)",
    "PC-MEDIA-WRAPPER": "언론사 바로가기",
    "PC-FEED-WRAPPER": "관심사 피드",
    "PC-MYSERVICE-META": "내 서비스",
}

_ASSIGN_RE = re.compile(
    r'window\[["\']EAGER-DATA["\']\]\[["\']([^"\']+)["\']\]\s*=\s*(\{)')

_MAX_ITEMS_PER_SECTION = 260
_LIST_STYLE_THRESHOLD = 20  # more items than this render as a compact row


def _escape(s):
    return (s.replace("&", "&amp;").replace("<", "&lt;")
            .replace(">", "&gt;").replace('"', "&quot;"))


def _hunt(node, out, depth=0):
    """Collect (title, url) pairs from arbitrary JSON."""
    if depth > 12 or len(out) >= _MAX_ITEMS_PER_SECTION:
        return
    if isinstance(node, dict):
        title = node.get("title") or node.get("text")
        url = (node.get("url") or node.get("linkUrl")
               or node.get("link"))
        if (isinstance(title, str) and len(title.strip()) > 4
                and isinstance(url, str) and url.startswith("http")):
            out.append((title.strip(), url))
        elif (isinstance(node.get("name"), str)
              and len(node["name"].strip()) > 1
              and isinstance(node.get("url"), str)
              and node["url"].startswith("http")):
            out.append((node["name"].strip(), node["url"]))
        for v in node.values():
            _hunt(v, out, depth + 1)
    elif isinstance(node, list):
        for v in node:
            _hunt(v, out, depth + 1)


def extract_sections(html):
    """[(section_key, [(title, url), ...]), ...] from EAGER-DATA blocks."""
    sections = []
    decoder = json.JSONDecoder()
    for m in _ASSIGN_RE.finditer(html):
        key = m.group(1)
        if any(part in key for part in _SKIP_KEY_PARTS):
            continue
        try:
            obj, _ = decoder.raw_decode(html[m.start(2):])
        except ValueError:
            continue
        items = []
        _hunt(obj, items)
        seen = set()
        uniq = []
        for t, u in items:
            if t in seen:
                continue
            seen.add(t)
            uniq.append((t, u))
        if uniq:
            sections.append((key, uniq))
    return sections


def reader_html(sections):
    """A self-styled block of the extracted content (empty if none)."""
    if not sections:
        return ""
    parts = [
        '<div id="gg-reader" style="margin-top:24px; padding:20px; '
        'background-color:#ffffff; border-width:1px; '
        'border-color:#e4e8eb; border-radius:16px">',
        '<p style="color:#03c75a; font-weight:bold; font-size:14px">'
        'GG 리더 모드 &#8212; 페이지 데이터에서 추출한 콘텐츠</p>',
    ]
    for key, items in sections:
        title = _TITLES.get(key, key)
        parts.append(
            f'<h2 style="font-size:18px; margin-top:18px">'
            f'{_escape(title)}</h2>')
        if len(items) <= _LIST_STYLE_THRESHOLD:
            parts.append("<ul>")
            for t, u in items:
                parts.append(
                    f'<li style="margin-top:6px">'
                    f'<a href="{_escape(u)}">{_escape(t)}</a></li>')
            parts.append("</ul>")
        else:
            row = " &#183; ".join(
                f'<a href="{_escape(u)}" style="color:#555555">'
                f'{_escape(t)}</a>'
                for t, u in items)
            parts.append(
                f'<p style="line-height:1.9; font-size:13px">{row}</p>')
    parts.append("</div>")
    return "".join(parts)


def inject(html):
    """Return html with the reader section before </body> (or appended).
    No-op when the page has no minable data."""
    try:
        block = reader_html(extract_sections(html))
    except Exception:
        return html  # reader mode must never break a page load
    if not block:
        return html
    m = re.search(r"</body\s*>", html, re.I)
    if m:
        return html[:m.start()] + block + html[m.start():]
    return html + block
