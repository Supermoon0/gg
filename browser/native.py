"""Optional native (Rust) fast path: HTML parse + CSS + style + JS.

If the compiled ggcore module is importable, the browser routes the
front half of the pipeline through Rust (including running scripts
in the embedded Boa JS engine). Otherwise everything falls back to
the pure-Python implementation transparently (without JS).
"""

try:
    import ggcore
except ImportError:
    ggcore = None
else:
    # 휠이 설치되지 않은 채 프로젝트 루트에서 실행하면 러스트 소스 디렉토리
    # ggcore/ 가 네임스페이스 패키지로 잡힌다 — 진짜 확장 모듈인지 확인.
    if not hasattr(ggcore, "parse_html"):
        ggcore = None

import os

from .html_parser import Element, Text, tree_to_list
from .style import DEFAULT_STYLE_SHEET


def available():
    return ggcore is not None


def build_tree(flat):
    """Rebuild a Python Element/Text tree from a Rust export dump."""
    root = None
    nodes = []
    for parent_idx, ridx, tag, text, attrs, style_pairs in flat:
        parent = nodes[parent_idx] if parent_idx >= 0 else None
        if tag is None:
            node = Text(text, parent)
        else:
            node = Element(tag, dict(attrs), parent)
        node.style = dict(style_pairs)
        node._font = None
        node._ridx = ridx
        nodes.append(node)
        if parent is not None:
            parent.children.append(node)
        else:
            root = node
    return root


def load_document(html, fetch_css, fetch_js=None, js_budget=3.0,
                  page_url=None):
    """Full native front half: parse -> scripts -> styles -> tree.

    fetch_css(hrefs) / fetch_js(srcs) -> {url: text} keep networking
    in Python. Returns (root, doc_handle, css_sources, js_console).

    js_budget: wall-clock seconds allowed for page scripts, checked
    between scripts. Heavy sites (naver, news portals) ship hundreds
    of KB of JS; without a budget the load stalls for minutes inside
    the JS engine. When exceeded, remaining scripts are skipped and
    the page renders with whatever ran — degraded, never frozen.
    """
    import threading
    import time

    # reader mode: app-shell pages carry their content as inline JSON;
    # surface it as ordinary markup before parsing (no-op elsewhere).
    # The JS engine now renders naver's feed natively (real card grid with
    # thumbnails), and the injected reader section pushes the feed's lazy
    # observers off-screen so they never mount — so reader mode is now an
    # opt-in fallback (GG_READER=1), not the default path.
    if os.environ.get("GG_READER") == "1":
        from . import reader
        html = reader.inject(html)

    doc = ggcore.parse_html(html)

    # overlap: stylesheets download while scripts fetch and run.
    # fetch_css must be UI-silent (it runs off the main thread); results
    # land in prefetched and net's cache, so the post-JS pass is free.
    pre_hrefs = [v for k, v in doc.stylesheet_entries() if k == "link"]
    prefetched = {}
    warm = None
    if pre_hrefs:
        def _warm():
            try:
                prefetched.update(fetch_css(pre_hrefs))
            except Exception:
                pass
        warm = threading.Thread(target=_warm, daemon=True)
        warm.start()

    logs = []
    if fetch_js is not None:
        entries = doc.script_entries()
        srcs = [value for kind, value in entries if kind == "src"]
        fetched = fetch_js(srcs) if srcs else {}
        sources = []
        for kind, value in entries:
            code = value if kind == "inline" else fetched.get(value, "")
            if code:
                sources.append(code)
        # gg-js has execution fuel (a runaway script is killed at its
        # instruction budget), so it may run scripts of any size. Boa
        # has no interrupt — keep the size guard there.
        fuel_safe = os.environ.get("GGJS") == "1"
        if page_url:
            try:
                doc.set_page_url(str(page_url))
            except Exception:
                pass  # older wheels have no set_page_url
        deadline = time.perf_counter() + js_budget
        for i, code in enumerate(sources):
            if not fuel_safe and len(code) > 400_000:
                # a mega-bundle can burn minutes inside one Boa call
                # (no engine interrupt) — skip it, keep the page
                logs.append(f"[gg] skipped a {len(code)//1024} KB script "
                            "(too large for the JS budget)")
                continue
            logs.extend(doc.run_scripts([code]))
            if time.perf_counter() > deadline:
                skipped = len(sources) - i - 1
                if skipped:
                    logs.append(
                        f"[gg] JS budget ({js_budget:.0f}s) exceeded - "
                        f"skipped {skipped} remaining script(s)")
                break
        # all scripts ran: fire DOMContentLoaded / load — app bundles
        # bootstrap from these
        if hasattr(doc, "fire_lifecycle"):
            try:
                logs.extend(doc.fire_lifecycle())
            except Exception:
                pass

    entries = doc.stylesheet_entries()
    hrefs = [value for kind, value in entries if kind == "link"]
    if warm is not None:
        warm.join()
    fetched = dict(prefetched)
    missing = [h for h in hrefs if h not in fetched]
    if missing:
        fetched.update(fetch_css(missing))
    css_sources = [DEFAULT_STYLE_SHEET]
    for kind, value in entries:
        css = value if kind == "inline" else fetched.get(value, "")
        if css:
            css_sources.append(css)

    doc.compute_styles(css_sources)
    return build_tree(doc.export()), doc, css_sources, logs


def refresh(doc, css_sources):
    """Re-style and re-export after JS mutated the DOM."""
    doc.compute_styles(css_sources)
    return build_tree(doc.export())


def restyle_patch(doc, css_sources, root):
    """Partial invalidation: recompute styles in Rust and, when only
    paint-affecting properties changed, patch them into the existing
    Python tree in place. Returns the least work the caller must
    redo: 'none' | 'paint' | 'layout' | 'tree'. For 'layout'/'tree'
    the Rust styles are already fresh — re-export with
    build_tree(doc.export()), do not compute_styles again."""
    if not hasattr(doc, "restyle_diff"):
        doc.compute_styles(css_sources)
        return "tree"
    kind, patches = doc.restyle_diff(css_sources)
    if kind == 0:
        return "none"
    if kind == 2:
        return "layout"
    if kind == 3:
        return "tree"
    by_ridx = {}
    for n in tree_to_list(root, []):
        r = getattr(n, "_ridx", None)
        if r is not None:
            by_ridx[r] = n
    for ridx, style_pairs in patches:
        n = by_ridx.get(ridx)
        if n is None:
            return "tree"  # tree out of sync: rebuild
        n.style = dict(style_pairs)
        n._font = None  # style changed: invalidate the font cache
    return "paint"


def async_available():
    """The gg-js event loop (pump/fetch) is present in this wheel."""
    return ggcore is not None and hasattr(ggcore.Doc, "pump")


def settle_async(doc, css_sources, base_url, timeout=8.0, max_rounds=2000):
    """Drive the gg-js event loop to quiescence: drain microtasks, fire
    virtual-clock timers, and service fetch() over the real network,
    looping until no work remains or the timeout. This is what makes
    fetch/setTimeout-driven SPA content materialize. Returns True if the
    DOM was mutated (caller should re-style/re-layout). No-op unless the
    async runtime is present."""
    if not async_available() or not hasattr(doc, "pump"):
        return False
    from . import net
    import time
    deadline = time.monotonic() + timeout
    mutated = False
    for _ in range(max_rounds):
        logs, fetches = doc.pump()
        if logs:
            for line in logs:
                print(f"[js console] {line}")
            mutated = True
        if fetches:
            mutated = True
            for fetch_id, url in fetches:
                try:
                    _h, body, _f = net.request_text(base_url.resolve(url))
                    doc.resolve_fetch(fetch_id, 200, body)
                except Exception as e:
                    doc.reject_fetch(fetch_id, f"{type(e).__name__}: {e}")
            continue  # resolving fetches queues more microtasks
        if not doc.has_pending_work():
            break
        if time.monotonic() > deadline:
            print("[js] event loop settle timed out")
            break
    return mutated


def parse_and_style(html, fetch_stylesheets):
    """Compatibility helper: parse + style without running JS."""
    root, _doc, _css, _logs = load_document(html, fetch_stylesheets)
    return root
