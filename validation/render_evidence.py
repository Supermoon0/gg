# -*- coding: utf-8 -*-
"""실증: 네이티브(Rust) 래스터 경로로 페이지를 헤드리스 렌더해 PNG 증거 생성.

shell.py의 render_frame 경로를 그대로 재현한다:
load_document(gg-js) → settle → refresh → DocumentLayout → paint_tree
→ TextEngine.set_display_list → render_frame(PPM) → PNG.

실행: xvfb-run -a python3 validation/render_evidence.py  (Pillow 필요)
결과: validation/out/evidence_*.png
"""
import io
import os
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO_ROOT = os.path.dirname(HERE)
sys.path.insert(0, REPO_ROOT)

from PIL import Image

from browser import native, net, textengine
from browser.draw import scale_cmds
from browser.layout import DocumentLayout, paint_tree

OUT = os.path.join(HERE, "out")
W = 1280

assert native.available(), "native ggcore wheel not importable"
engine = textengine.engine()
assert engine is not None, "TextEngine missing"


def fetch_many(urls, base, binary=False):
    out = {}
    for u in urls:
        try:
            resolved = base.resolve(u)
            if binary:
                out[u] = net.request_raw(
                    resolved, site_for_cookies=base,
                    top_level_navigation=False)[1]
            else:
                out[u] = net.request_text(
                    resolved, site_for_cookies=base,
                    top_level_navigation=False)[1]
        except Exception:
            out[u] = b"" if binary else ""
    return out


def render(name, url_str, height=800, expected_text=(), forbidden_text=(),
           advance_ms=0.0):
    t0 = time.perf_counter()
    url = net.URL(url_str)
    _h, body, url = net.request_text(url)
    nodes, doc, css_sources, logs = native.load_document(
        body,
        lambda hrefs: fetch_many(hrefs, url),
        lambda srcs: fetch_many(srcs, url),
        page_url=url)
    if native.settle_async(doc, css_sources, url):
        nodes = native.refresh(doc, css_sources)
    if advance_ms > 0.0 and hasattr(doc, "tick"):
        version_before = doc.dom_version() if hasattr(doc, "dom_version") else None
        live_logs, fetches = native.pump_script_requests(doc, advance_ms)
        logs.extend(live_logs)
        native.sync_cookie_writes(doc, url)
        for request in fetches:
            native.service_script_fetch(doc, url, request)
        if fetches:
            native.settle_async(doc, css_sources, url)
        if version_before is None or doc.dom_version() != version_before:
            nodes = native.refresh(doc, css_sources)
    engine.clear_images()
    from browser.html_parser import Element, tree_to_list
    img_nodes = [n for n in tree_to_list(nodes, [])
                 if isinstance(n, Element) and n.tag == "img"
                 and n.attributes.get("src")]
    if img_nodes:
        raw = fetch_many([n.attributes["src"] for n in img_nodes], url,
                         binary=True)
        for n in img_nodes:
            data = raw.get(n.attributes["src"]) or b""
            try:
                n._img = engine.load_image(data) if data else None
            except Exception:
                n._img = None
    textengine.load_svgs(nodes)
    textengine.load_background_images(
        nodes, lambda urls: fetch_many(urls, url, binary=True))

    d = DocumentLayout(nodes)
    d.layout(W, height)
    cmds = paint_tree(d, [])
    painted_text = " ".join(
        c.text for c in cmds if hasattr(c, "text") and c.text)
    missing = [text for text in expected_text if text not in painted_text]
    stale = [text for text in forbidden_text if text in painted_text]
    if missing or stale:
        raise AssertionError(
            f"{name} dynamic paint mismatch: missing={missing}, stale={stale}")
    page_h = min(max(int(d.height) + 40, 200), height)
    engine.set_display_list(scale_cmds(
        [c.native(0, 0.0) for c in cmds], 1.0))
    ppm = engine.render_frame(W, page_h, (255, 255, 255), 0.0, 0.0, [])
    img = Image.open(io.BytesIO(bytes(ppm)))
    os.makedirs(OUT, exist_ok=True)
    path = os.path.join(OUT, f"evidence_{name}.png")
    img.save(path)
    dt = time.perf_counter() - t0
    texts = [c for c in cmds if hasattr(c, "text")]
    print(f"[OK] {name}: {len(cmds)} paint cmds, {len(texts)} text cmds, "
          f"doc height {d.height:.0f}px, {dt*1000:.0f}ms -> {path}")
    for line in logs[:8]:
        print(f"     [js] {line}")
    return path


if __name__ == "__main__":
    targets = sys.argv[1:] or ["home", "demo", "css", "js"]
    if "home" in targets:
        render("home", "about:home")
    if "demo" in targets:
        render("demo", "file://" + os.path.join(REPO_ROOT, "demo_live.html"),
               expected_text=("✔",), forbidden_text=("3초 후 이 문장",),
               advance_ms=3100.0)
    if "css" in targets:
        render("css", "file://" + os.path.join(HERE, "fixture_css.html"),
               height=2100)
    if "js" in targets:
        render("js", "file://" + os.path.join(HERE, "fixture_js.html"))
