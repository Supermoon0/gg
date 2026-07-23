# -*- coding: utf-8 -*-
"""Measure each host/JS event-loop turn while loading a real page.

Set GG_JS_PROFILE=1 to include sampled hot JavaScript functions.  Unlike the
render audit, this deliberately avoids doing layout so a long JS callback is
reported against the exact pump turn that ran it.
"""
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

try:
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
except Exception:
    pass

from browser import native, net  # noqa: E402


def fetch_many(base, urls):
    out = {}
    for url in urls:
        try:
            out[url] = net.request_text(
                base.resolve(url), site_for_cookies=base,
                top_level_navigation=False)[1]
        except Exception:
            out[url] = ""
    return out


def profile_rows(doc, limit=15):
    if not hasattr(doc, "take_js_profile"):
        return []
    return [
        {"name": name, "module": module, "proto": proto,
         "estimated_instructions": instructions}
        for name, module, proto, instructions in doc.take_js_profile()[:limit]
    ]


def main(url_text):
    started = time.perf_counter()
    base = net.URL(url_text)
    _headers, body, base = net.request_text(base, no_cache=True)
    timings = {}
    nodes, doc, css, logs = native.load_document(
        body, lambda urls: fetch_many(base, urls),
        lambda urls: fetch_many(base, urls), js_budget=25.0,
        page_url=base, timings=timings)
    print(json.dumps({
        "phase": "initial", "ms": round((time.perf_counter() - started) * 1000),
        "nodes": len(native.flatten(nodes)), "stages_ms": {
            key: round(value) for key, value in timings.items()},
        "logs": [str(line) for line in logs[-8:]],
        "profile": profile_rows(doc),
    }, ensure_ascii=False), flush=True)

    max_turns = int(os.environ.get("GG_PROBE_TURNS", "12"))
    for turn in range(max_turns):
        turn_started = time.perf_counter()
        turn_logs, fetches = native.pump_script_requests(doc)
        pump_ms = (time.perf_counter() - turn_started) * 1000
        fetch_rows = []
        for request in fetches:
            fetch_started = time.perf_counter()
            native.service_script_fetch(doc, base, request)
            fetch_rows.append({
                "url": str(request[1])[:180],
                "ms": round((time.perf_counter() - fetch_started) * 1000),
            })
        nodes = native.refresh(doc, css)
        print(json.dumps({
            "phase": "pump", "turn": turn, "pump_ms": round(pump_ms),
            "fetches": fetch_rows, "nodes": len(native.flatten(nodes)),
            "pending": bool(doc.has_pending_work()),
            "logs": [str(line) for line in turn_logs[-8:]],
            "profile": profile_rows(doc),
        }, ensure_ascii=False), flush=True)
        if not fetches and not doc.has_pending_work():
            break


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else "https://www.naver.com/")
