# -*- coding: utf-8 -*-
"""Generic, site-agnostic layout audit: render a URL headless and report
objective layout-health metrics as one JSON line. Not tuned for any site.

Metrics:
  nodes        DOM nodes after settle
  paint        paint commands
  text         text draw commands
  images       <img> nodes with a src
  js_errors    distinct "[gg-js error]" / uncaught console errors
  overlaps     pairs of text runs that visually overlap (real widths) —
               the primary layout-defect signal
  collapsed    in-flow block containers with height 0 but visible text
               descendants (a classic "layout dropped it" symptom)
  offscreen    text runs painted at x<0 or beyond the viewport width
  height       document height
  ms           wall-clock render time

Usage: python render_audit.py <url> [width]
"""
import io
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

try:  # windows consoles default to cp949; page text is utf-8
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
except Exception:
    pass

from browser import native, net  # noqa: E402
from browser.layout import (DocumentLayout, BlockLayout, TextLayout,  # noqa: E402
                            paint_tree)
from browser.html_parser import Element, Text, tree_to_list  # noqa: E402

W = int(sys.argv[2]) if len(sys.argv) > 2 else 1280
SETTLE_ROUNDS = int(os.environ.get("GG_AUDIT_SETTLE_ROUNDS", "3"))


def fetch_many(base, urls, binary=False):
    out = {}
    for u in urls:
        try:
            out[u] = (net.request_raw(
                base.resolve(u), site_for_cookies=base,
                top_level_navigation=False)[1] if binary
                else net.request_text(
                    base.resolve(u), site_for_cookies=base,
                    top_level_navigation=False)[1])
        except Exception:
            out[u] = b"" if binary else ""
    return out


def walk(box, out):
    out.append(box)
    for c in getattr(box, "children", []):
        walk(c, out)
    return out


def audit(url_str):
    t0 = time.perf_counter()
    base = net.URL(url_str)
    _h, body, base = net.request_text(base, no_cache=True)
    network_ms = (time.perf_counter() - t0) * 1000.0
    load_timings = {}
    nodes, doc, css, logs = native.load_document(
        body, lambda h: fetch_many(base, h),
        lambda s: fetch_many(base, s), js_budget=25.0, page_url=base,
        timings=load_timings)
    initial_ms = (time.perf_counter() - t0) * 1000.0
    settle_started = time.perf_counter()
    # bounded settle so JS apps mount without hanging
    for rnd in range(SETTLE_ROUNDS):
        try:
            native.settle_async(doc, css, base, timeout=15.0)
        except Exception:
            break
        nodes = native.refresh(doc, css)
        for _ in range(30):
            _pl, fetches = native.pump_script_requests(doc)
            if fetches:
                for request in fetches:
                    native.service_script_fetch(doc, base, request)
                continue
            if not doc.has_pending_work():
                break
        if rnd >= 1 and not doc.has_pending_work():
            break
    nodes = native.refresh(doc, css)
    settle_ms = (time.perf_counter() - settle_started) * 1000.0
    flat = tree_to_list(nodes, [])
    d = DocumentLayout(nodes)
    d.layout(W, 6000)
    cmds = paint_tree(d, [])
    boxes = walk(d, [])

    # --- overlaps: real text-run widths ---
    tl = [(b.x, b.y, b.width, getattr(b, "height", 16), b.word)
          for b in boxes
          if isinstance(b, TextLayout) and (b.word or "").strip()]
    tl.sort(key=lambda r: (round(r[1] / 8), r[0]))
    overlaps = 0
    samples = []
    for i in range(len(tl)):
        x1, y1, w1, h1, t1 = tl[i]
        # text whose center sits above the viewport is clipped and not
        # visible — e.g. `position:absolute; top:-30px` sr-only skip links
        # and focus-revealed menus. Overlaps among hidden runs are not
        # visible defects, so don't count them (they otherwise dominate).
        if y1 + h1 / 2 <= 0:
            continue
        for j in range(i + 1, len(tl)):
            x2, y2, w2, h2, t2 = tl[j]
            if y2 + h2 / 2 <= 0:
                continue
            if y2 - y1 > max(h1, 6):
                break
            if (abs(y1 - y2) < max(h1, h2) * 0.6
                    and x1 < x2 + w2 - 6 and x2 < x1 + w1 - 6):
                overlaps += 1
                if len(samples) < 8:
                    samples.append(
                        [round(y1), t1[:10], t2[:10]])

    # --- collapsed containers: block boxes h==0 with visible text inside ---
    collapsed = 0
    for b in boxes:
        if isinstance(b, BlockLayout) and getattr(b, "height", 1) == 0:
            has_text = any(isinstance(c, TextLayout) and (c.word or "").strip()
                           for c in walk(b, []))
            if has_text:
                collapsed += 1

    # --- offscreen text ---
    offscreen = sum(1 for (x, y, w, h, t) in tl if x < -2 or x > W + 2)

    imgs = sum(1 for n in flat
               if isinstance(n, Element) and n.tag == "img"
               and n.attributes.get("src"))
    error_logs = [str(line) for line in logs
                  if "gg-js error" in str(line)
                  or "uncaught" in str(line).lower()]
    js_err = len(error_logs)
    return {
        "url": url_str,
        "nodes": len(flat),
        "paint": len(cmds),
        "text": sum(1 for c in cmds if hasattr(c, "text")),
        "images": imgs,
        "js_errors": js_err,
        "js_error_samples": error_logs[:5],
        "overlaps": overlaps,
        "collapsed": collapsed,
        "offscreen": offscreen,
        "height": round(d.height),
        "ms": round((time.perf_counter() - t0) * 1000),
        "network_ms": round(network_ms),
        "initial_ms": round(initial_ms),
        "settle_ms": round(settle_ms),
        "stages_ms": {key: round(value) for key, value in
                      load_timings.items()},
        "overlap_samples": samples,
    }


if __name__ == "__main__":
    try:
        print(json.dumps(audit(sys.argv[1]), ensure_ascii=False))
    except Exception as e:
        import traceback
        print(json.dumps({"url": sys.argv[1], "error": str(e),
                          "trace": traceback.format_exc()[-400:]},
                         ensure_ascii=False))
