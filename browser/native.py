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
    # surface it as ordinary markup before parsing (no-op elsewhere)
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
        srcs = [value for kind, value in entries
                if kind in ("src", "msrc")]
        fetched = fetch_js(srcs) if srcs else {}
        sources = []
        for kind, value in entries:
            code = (value if kind in ("inline", "minline")
                    else fetched.get(value, ""))
            if not code:
                continue
            if kind in ("minline", "msrc"):
                # <script type=module>: run the static import linker
                # so import/export never reach the classic parser
                code = _link_module(
                    code, kind, value, page_url, fetch_js, logs)
            sources.append(code)
        # gg-js has execution fuel (a runaway script is killed at its
        # instruction budget), so it may run scripts of any size. Boa
        # has no interrupt — keep the size guard there.
        import os
        fuel_safe = os.environ.get("GGJS") == "1"
        if page_url:
            try:
                doc.set_page_url(str(page_url))
            except Exception:
                pass  # older wheels have no set_page_url
        _seed_page_cookies(doc, page_url)
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
        _pull_page_cookies(doc, page_url)

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
    if hasattr(doc, "take_mutated"):
        doc.take_mutated()  # the full export below covers everything
    return build_tree(doc.export()), doc, css_sources, logs


def _link_module(code, kind, value, page_url, fetch_js, logs):
    """Link a <script type=module> with the static linker; on any
    failure the original source runs (and errors) unmodified."""
    from . import esmodules, net

    if page_url is not None and kind == "msrc":
        try:
            base = page_url.resolve(value)
        except Exception:
            base = page_url
    else:
        base = page_url

    def loader(spec, base_url):
        try:
            resolved = (base_url.resolve(spec) if base_url is not None
                        else net.URL(spec))
            _h, text, _f = net.request_text(resolved)
            return text, str(resolved)
        except Exception as e:
            logs.append(f"[gg] module fetch failed: {spec} "
                        f"({type(e).__name__})")
            return None, spec

    try:
        return esmodules.link(code, base, loader)
    except Exception as e:
        logs.append(f"[gg] module link failed ({type(e).__name__}: {e})"
                    " - running unlinked")
        return code


def _seed_page_cookies(doc, page_url):
    """document.cookie starts with this host's network-jar cookies."""
    if page_url is None or not hasattr(doc, "set_cookies"):
        return
    try:
        from . import net
        host = net.URL(str(page_url)).host
        pairs = list(net.cookies_for(host).items())
        if pairs:
            doc.set_cookies(pairs)
    except Exception:
        pass


def _pull_page_cookies(doc, page_url):
    """JS document.cookie writes flow back into the network jar so
    later requests to this host carry them."""
    if page_url is None or not hasattr(doc, "get_cookies"):
        return
    try:
        from . import net
        host = net.URL(str(page_url)).host
        pairs = doc.get_cookies()
        if pairs:
            net.seed_cookies(host, pairs)
    except Exception:
        pass


def refresh(doc, css_sources):
    """Re-style and re-export after JS mutated the DOM."""
    doc.compute_styles(css_sources)
    if hasattr(doc, "take_mutated"):
        doc.take_mutated()  # full rebuild covers everything: drain
    return build_tree(doc.export())


# more mutated subtrees than this per tick -> a full rebuild is
# cheaper than many splices (and React's initial mount IS a full tree)
_PARTIAL_MAX_SUBTREES = 32


def refresh_partial(doc, css_sources, root):
    """N3 v1: refresh the Python tree after JS mutations with the
    least marshaling. Styles recompute in Rust (fast); then, when few
    subtrees changed, only those are re-exported and spliced in place
    — the full-DOM marshal (build_tree(export()), the dominant cost
    on big pages) is skipped. Any doubt falls back to the full
    rebuild (never silently wrong). Returns the tree root.

    v1 gap: unmutated nodes keep their previous style copies; a
    mutation that restyles distant nodes (sibling combinators) shows
    stale style until the next full refresh. hover/focus restyles
    take the accurate restyle_patch path instead."""
    if root is None or not hasattr(doc, "take_mutated") \
            or not hasattr(doc, "export_subtree"):
        return refresh(doc, css_sources)
    doc.compute_styles(css_sources)
    mutated = doc.take_mutated()
    if not mutated or len(mutated) > _PARTIAL_MAX_SUBTREES:
        return build_tree(doc.export())
    by_ridx = {}
    for n in tree_to_list(root, []):
        r = getattr(n, "_ridx", None)
        if r is not None:
            by_ridx[r] = n
    mset = set(mutated)
    tops = []
    for ridx in mutated:
        node = by_ridx.get(ridx)
        if node is None:
            # a node created this tick under a mutated parent: its
            # parent is in the set too, so it's covered — but a
            # mutated node we can't locate at all means the trees
            # diverged: rebuild
            continue
        anc = node.parent
        covered = False
        while anc is not None:
            if getattr(anc, "_ridx", None) in mset:
                covered = True
                break
            anc = anc.parent
        if not covered:
            tops.append(node)
    if not tops:
        return build_tree(doc.export())
    for old in tops:
        flat = doc.export_subtree(old._ridx)
        if not flat:
            return build_tree(doc.export())
        sub = build_tree(flat)
        parent = old.parent
        sub.parent = parent
        if parent is None:
            return sub  # the root itself mutated
        try:
            i = parent.children.index(old)
        except ValueError:
            return build_tree(doc.export())
        parent.children[i] = sub
    return root


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
    injected = 0
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
                    _h, body, _f = net.fetch_for_page(
                        base_url, base_url.resolve(url))
                    doc.resolve_fetch(fetch_id, 200, body)
                except Exception as e:
                    doc.reject_fetch(fetch_id, f"{type(e).__name__}: {e}")
            continue  # resolving fetches queues more microtasks
        ran, injected = _drain_injected_scripts(
            doc, base_url, injected)
        if ran:
            mutated = True
            continue  # injected code may queue more work
        if not doc.has_pending_work():
            break
        if time.monotonic() > deadline:
            print("[js] event loop settle timed out")
            break
    _pull_page_cookies(doc, base_url)
    return mutated


# a page injecting more scripts than this per settle is treated as a
# runaway ad chain, not an app boot (design risk table)
_MAX_INJECTED_SCRIPTS = 50


def _drain_injected_scripts(doc, base_url, injected):
    """N1: fetch+run <script> elements the page injected since the
    last drain. External srcs resolve against the page URL; a script
    fires load on success and error on fetch failure (getScript-style
    chaining). Returns (ran_any, new_injected_count)."""
    if not hasattr(doc, "take_pending_scripts"):
        return False, injected
    from . import net
    ext, inline = doc.take_pending_scripts()
    if not ext and not inline:
        return False, injected
    ran = False
    for _node, code in inline:
        if injected >= _MAX_INJECTED_SCRIPTS:
            break
        injected += 1
        ran = True
        for line in doc.run_scripts([code]):
            print(f"[js console] {line}")
    for node, src in ext:
        if injected >= _MAX_INJECTED_SCRIPTS:
            print(f"[gg] injected-script cap reached, skipping {src}")
            continue
        injected += 1
        ran = True
        try:
            resolved = (base_url.resolve(src)
                        if base_url is not None else net.URL(src))
            _h, code, _f = net.request_text(resolved)
        except Exception as e:
            print(f"[gg] injected script fetch failed: {src} "
                  f"({type(e).__name__})")
            for line in doc.fire_node_event(node, "error"):
                print(f"[js console] {line}")
            continue
        for line in doc.run_scripts([code]):
            print(f"[js console] {line}")
        for line in doc.fire_node_event(node, "load"):
            print(f"[js console] {line}")
    return ran, injected


def parse_and_style(html, fetch_stylesheets):
    """Compatibility helper: parse + style without running JS."""
    root, _doc, _css, _logs = load_document(html, fetch_stylesheets)
    return root
