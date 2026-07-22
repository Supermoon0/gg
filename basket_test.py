# -*- coding: utf-8 -*-
"""Site basket: render representative pages headless and score how far
the engine gets (network, parse, style, layout, paint, JS errors)."""
import os, sys, time, traceback
sys.path.insert(0, r"E:\gg")
sys.stdout.reconfigure(encoding="utf-8", errors="replace")
from concurrent.futures import ThreadPoolExecutor

import tkinter
from browser import net, native
from browser.html_parser import Element, Text, tree_to_list

SITES = [
    ("네이버", "https://www.naver.com/"),
    ("위키백과", "https://ko.wikipedia.org/wiki/웹_브라우저"),
    ("나무위키", "https://namu.wiki/w/월드_와이드_웹"),
    ("연합뉴스", "https://www.yna.co.kr/"),
    ("티스토리", "https://www.tistory.com/"),
    ("HN", "https://news.ycombinator.com/"),
    ("MDN", "https://developer.mozilla.org/en-US/"),
    ("example", "https://example.com/"),
    ("정부24", "https://www.gov.kr/portal/main"),
]

root = tkinter.Tk(); root.withdraw()
os.environ["GGJS"] = "1"

def fetch_many(base, urls, binary=False):
    def one(u):
        try:
            if binary:
                return net.request_raw(
                    base.resolve(u), site_for_cookies=base,
                    top_level_navigation=False)[1]
            return net.request_text(
                base.resolve(u), site_for_cookies=base,
                top_level_navigation=False)[1]
        except Exception:
            return b"" if binary else ""
    with ThreadPoolExecutor(max_workers=6) as pool:
        return dict(zip(urls, pool.map(one, urls)))

from browser.layout import DocumentLayout, paint_tree

print(f"{'사이트':10} {'노드':>6} {'스타일':>6} {'페인트':>6} "
      f"{'텍스트':>6} {'이미지':>5} {'JS오류':>6} {'시간':>6}  결과")
print("-" * 78)
for name, url in SITES:
    t0 = time.perf_counter()
    try:
        base = net.URL(url)
        _h, body, final = net.request_text(base, no_cache=True)
        base = final
        def fc(hrefs): return fetch_many(base, hrefs)
        def fj(srcs): return fetch_many(base, srcs)
        nodes, doc, css_sources, logs = native.load_document(body, fc, fj)
        flat = tree_to_list(nodes, [])
        elems = [n for n in flat if isinstance(n, Element)]
        styled = sum(1 for n in elems
                     if n.style.get("color") not in (None, "black")
                     or n.style.get("background-color"))
        d = DocumentLayout(nodes)
        d.layout(1280, 800)
        cmds = paint_tree(d, [])
        texts = [c for c in cmds if hasattr(c, "text")]
        imgs = [c for c in cmds if hasattr(c, "image_id")]
        js_err = sum(1 for l in logs if "error" in l.lower())
        dt = time.perf_counter() - t0
        verdict = ("읽을만함" if len(texts) > 40 and d.height > 300
                   else "빈약" if len(texts) > 3 else "실패")
        print(f"{name:10} {len(elems):>6} {styled:>6} {len(cmds):>6} "
              f"{len(texts):>6} {len(imgs):>5} {js_err:>6} "
              f"{dt:>5.1f}s  {verdict}")
    except Exception as e:
        dt = time.perf_counter() - t0
        print(f"{name:10} {'—':>6} {'—':>6} {'—':>6} {'—':>6} {'—':>5} "
              f"{'—':>6} {dt:>5.1f}s  예외: {type(e).__name__}: {str(e)[:40]}")
root.destroy()
