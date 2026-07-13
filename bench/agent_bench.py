"""Agent-action end-to-end benchmark — the go/no-go gate for the
AI-native thesis.

An "agent action" is one perceive+act step: spin up a context, load a
page, extract a semantic snapshot of the interactive elements, and click
one. We time that whole round-trip two ways:

  * GG driver  — in-process (no CDP, no separate renderer)
  * Playwright — driving headless Edge WARM (browser launched once,
                 a cheap fresh context/page per action). This is
                 Chromium at its BEST for fleets, not a cold-start
                 strawman, so the comparison is honest.

Both load the SAME local file and extract the SAME info (role/name/href
for links, buttons, headings), then click the first button.

    python bench/agent_bench.py            # both, N=25
    python bench/agent_bench.py --n 50
    python bench/agent_bench.py --gg        # GG only
"""

import os
import statistics
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)


def make_page(n_items=150):
    """A realistic listing page: headings, links, buttons, plus inline JS
    that builds part of the content (so the JS engine is on the path)."""
    rows = []
    for i in range(n_items):
        rows.append(
            f'<div class="card"><h3>Item {i}</h3>'
            f'<a href="/item/{i}">open {i}</a>'
            f'<button class="buy" data-id="{i}">buy {i}</button></div>')
    static = "".join(rows)
    html = f"""<!doctype html><html><head><title>Listing</title></head>
<body>
<nav><a href="/home">Home</a><a href="/about">About</a></nav>
<h1>Product Listing</h1>
<div id="grid">{static}</div>
<div id="dynamic"></div>
<script>
var host = document.getElementById('dynamic');
for (var i = 0; i < 40; i++) {{
  var d = document.createElement('div');
  d.setAttribute('class', 'card');
  var h = document.createElement('h3');
  h.textContent = 'Dyn ' + i;
  d.appendChild(h);
  host.appendChild(d);
}}
</script>
</body></html>"""
    path = os.path.join(tempfile.mkdtemp(prefix="ggagent_"), "page.html")
    with open(path, "w", encoding="utf-8") as f:
        f.write(html)
    return path


def summarize(label, times_ms):
    med = statistics.median(times_ms)
    p10 = min(times_ms)
    print(f"  {label:22} median {med:8.2f} ms   best {p10:8.2f} ms"
          f"   (n={len(times_ms)})")
    return med


def bench_gg(file_path, n, engine="ggjs"):
    from browser.driver import Page

    url = "file:///" + file_path.replace("\\", "/")
    times = []
    counts = []
    for _ in range(n):
        t0 = time.perf_counter()
        page = Page(engine=engine)          # fresh in-process context
        page.goto(url)
        snap = page.snapshot()               # semantic extraction
        page.click(".buy")                   # act
        times.append((time.perf_counter() - t0) * 1000)
        counts.append(len(snap))
    print(f"  (extracted ~{counts[0]} interactive nodes/action)")
    return times


def bench_playwright(file_path, n):
    from playwright.sync_api import sync_playwright

    url = "file:///" + file_path.replace("\\", "/")
    times = []
    counts = []
    extract_js = (
        "els => els.map(e => ({role: e.tagName, "
        "name: e.textContent, href: e.getAttribute('href')}))")
    with sync_playwright() as p:
        # WARM: launch the installed Edge once; reuse across actions
        browser = p.chromium.launch(channel="msedge", headless=True)
        # one warmup (JIT/first-context costs excluded from the median)
        ctx = browser.new_context()
        pg = ctx.new_page()
        pg.goto(url)
        pg.eval_on_selector_all("a,button,h1,h2,h3", extract_js)
        ctx.close()
        for _ in range(n):
            t0 = time.perf_counter()
            ctx = browser.new_context()      # cheap fresh context
            pg = ctx.new_page()
            pg.goto(url)
            snap = pg.eval_on_selector_all("a,button,h1,h2,h3", extract_js)
            pg.click("button.buy")           # act
            ctx.close()
            times.append((time.perf_counter() - t0) * 1000)
            counts.append(len(snap))
        browser.close()
    print(f"  (extracted ~{counts[0]} interactive nodes/action)")
    return times


def bench_playwright_reuse(file_path, n):
    """Chromium at its ABSOLUTE cheapest: reuse one page, just navigate.
    No fresh context per action (so no per-task isolation), which is the
    lower bound on Playwright cost — included so the comparison cannot be
    accused of sandbagging."""
    from playwright.sync_api import sync_playwright

    url = "file:///" + file_path.replace("\\", "/")
    times = []
    extract_js = (
        "els => els.map(e => ({role: e.tagName, "
        "name: e.textContent, href: e.getAttribute('href')}))")
    with sync_playwright() as p:
        browser = p.chromium.launch(channel="msedge", headless=True)
        ctx = browser.new_context()
        pg = ctx.new_page()
        pg.goto(url)  # warmup
        pg.eval_on_selector_all("a,button,h1,h2,h3", extract_js)
        for _ in range(n):
            t0 = time.perf_counter()
            pg.goto(url)
            pg.eval_on_selector_all("a,button,h1,h2,h3", extract_js)
            pg.click("button.buy")
            times.append((time.perf_counter() - t0) * 1000)
        browser.close()
    return times


def _gate(label, ratio):
    verdict = "GATE PASSED (>=5x)" if ratio >= 5 else (
        "promising, below 5x kill line" if ratio >= 2 else "KILL SIGNAL (<2x)")
    print(f"  vs {label:28} GG {ratio:5.1f}x cheaper  -> {verdict}")


def main():
    args = sys.argv[1:]
    n = 25
    if "--n" in args:
        n = int(args[args.index("--n") + 1])
    do_gg = "--pw" not in args
    do_pw = "--gg" not in args

    file_path = make_page()
    print(f"Agent-action benchmark  (page: {file_path}, n={n})")
    print("One action = load + semantic snapshot + click\n")

    gg_med = pw_ctx = pw_reuse = None
    if do_gg:
        print("GG driver (in-process, gg-js engine):")
        gg_med = summarize("gg in-process", bench_gg(file_path, n))
        print()
    if do_pw:
        print("Playwright + Edge, WARM + fresh context/action (isolated):")
        try:
            pw_ctx = summarize("pw warm+ctx", bench_playwright(file_path, n))
        except Exception as e:
            print(f"  [playwright failed] {e}")
        print()
        print("Playwright + Edge, cheapest: ONE page reused, navigate-only:")
        try:
            pw_reuse = summarize("pw reuse", bench_playwright_reuse(file_path, n))
        except Exception as e:
            print(f"  [playwright failed] {e}")
        print()

    if gg_med and (pw_ctx or pw_reuse):
        print("=" * 60)
        print(f"  cost-per-agent-action: GG in-process = {gg_med:.1f} ms")
        if pw_ctx:
            _gate("Playwright fresh-context", pw_ctx / gg_med)
        if pw_reuse:
            _gate("Playwright page-reuse (floor)", pw_reuse / gg_med)
        print("=" * 60)


if __name__ == "__main__":
    main()


def make_async_page(n_items=120):
    """Same listing, but the cards arrive via fetch()+setTimeout — the
    SPA pattern, to confirm the cost gate holds with async on the path."""
    import json as _json
    items = [{"i": i, "name": f"Item {i}"} for i in range(n_items)]
    data_url = "data:application/json," + _json.dumps(items).replace(" ", "")
    html = f"""<!doctype html><html><head><title>Async Listing</title></head>
<body><h1>Product Listing</h1><div id="grid">loading</div>
<script>
fetch({_json.dumps(data_url)}).then(function (r) {{ return r.json(); }})
.then(function (items) {{
  setTimeout(function () {{
    var grid = document.getElementById('grid');
    grid.textContent = '';
    for (var i = 0; i < items.length; i++) {{
      var c = document.createElement('div'); c.setAttribute('class','card');
      var h = document.createElement('h3'); h.textContent = items[i].name;
      var a = document.createElement('a'); a.setAttribute('href','/item/'+i);
      a.textContent = 'open ' + i;
      var b = document.createElement('button'); b.setAttribute('class','buy');
      b.textContent = 'buy ' + i;
      c.appendChild(h); c.appendChild(a); c.appendChild(b); grid.appendChild(c);
    }}
  }}, 20);
}});
</script></body></html>"""
    import os as _os, tempfile as _t
    path = _os.path.join(_t.mkdtemp(prefix="ggasync_"), "page.html")
    with open(path, "w", encoding="utf-8") as f:
        f.write(html)
    return path


if __name__ == "__main__" and "--async" in __import__("sys").argv:
    import statistics, time
    fp = make_async_page()
    url = "file:///" + fp.replace("\\", "/")
    from browser.driver import Page
    times, counts = [], []
    for _ in range(25):
        t0 = time.perf_counter()
        p = Page(engine="ggjs")
        p.goto(url)                 # settles the fetch+timer SPA
        snap = p.snapshot()
        p.click(".buy")
        times.append((time.perf_counter() - t0) * 1000)
        counts.append(len(snap))
    print(f"async-SPA GG in-process: median {statistics.median(times):.1f} ms"
          f"  (~{counts[0]} interactive nodes materialized via fetch+timer)")
