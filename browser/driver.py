"""Headless automation driver — GG as an embeddable, agent-drivable
engine (no GUI window, no tkinter).

This is the seam that turns the engine into a library: an agent (or a
test) can navigate, query, read, click, and evaluate against a live
page entirely in-process, with no CDP round-trip and no renderer
process — the AI-native wedge.

    from browser.driver import Page
    page = Page()
    page.goto("https://example.com")
    print(page.title)
    print(page.text("h1"))
    for link in page.links():
        print(link.text, "->", link.href)

Selector resolution reuses the *engine's own* querySelector matcher
(via a tagging script), so whatever selectors the Rust/JS engine
supports, the driver supports — no second selector implementation to
drift. Requires the native ggcore wheel; raises if it is absent.
"""

import json
import os
from concurrent.futures import ThreadPoolExecutor

from . import native, net

_MARKER = "data-gg-hit"
_VAL_TAG = "\x01GGVAL\x01"

# Newer wheels expose Rust-side snapshot()/query() that skip the whole-
# tree Python marshal (the AI-native primitive). Probed once at import.
try:
    _HAS_NATIVE = (native.available()
                   and hasattr(native.ggcore.Doc, "snapshot")
                   and hasattr(native.ggcore.Doc, "query"))
except Exception:
    _HAS_NATIVE = False

# The async event loop (pump/resolve_fetch) is gg-js-only and needs a
# wheel that exposes it. settle() no-ops without it.
try:
    _HAS_PUMP = native.available() and hasattr(native.ggcore.Doc, "pump")
except Exception:
    _HAS_PUMP = False

_SETTLE_MAX_ROUNDS = 2000


class DriverError(RuntimeError):
    pass


class Element:
    """A lightweight handle to a matched DOM node (arena index _ridx)."""

    __slots__ = ("tag", "attrs", "text", "_ridx", "_page")

    def __init__(self, page, ridx, tag, text, attrs):
        self._page = page
        self._ridx = ridx
        self.tag = tag
        self.text = text
        self.attrs = attrs

    @property
    def href(self):
        return self.attrs.get("href")

    @property
    def id(self):
        return self.attrs.get("id")

    def attr(self, name):
        return self.attrs.get(name)

    def click(self):
        return self._page._click_element(self)

    def __repr__(self):
        t = (self.text or "").strip()
        if len(t) > 40:
            t = t[:37] + "..."
        return f"<{self.tag} #{self._ridx} {t!r}>"


class Page:
    """A single headless page/context over one native Doc handle."""

    def __init__(self, engine="boa", run_scripts=True, timeout=15):
        # engine: "boa" (broader coverage today) or "ggjs" (clean-room
        # engine, the strategic target). See docs/jsvm-research.md.
        if not native.available():
            raise DriverError(
                "native ggcore wheel not installed — the driver needs it")
        self.engine = engine
        self.run_scripts = run_scripts
        self.timeout = timeout
        self.url = None
        self.title = None
        self._doc = None
        self._css_sources = []
        self._console = []
        self._styles_dirty = False   # a handler mutated the DOM
        self._export_cache = None     # (version, flat); invalidated on mutation
        self._ver = 0

    # ---------- navigation ----------

    def goto(self, url_or_str, settle=True):
        """Fetch, parse, run page scripts, compute styles. When `settle`
        (default), also run the event loop to a fixed point so async
        content (fetch/setTimeout) is present before you read the page.
        Returns self."""
        url = url_or_str if isinstance(url_or_str, net.URL) \
            else net.URL(url_or_str)
        _headers, body, final_url = net.request_text(url, no_cache=True)
        self.url = final_url

        fetch_js = self._fetch_scripts if self.run_scripts else None
        prev = os.environ.get("GGJS")
        os.environ["GGJS"] = "1" if self.engine == "ggjs" else "0"
        try:
            _root, doc, css_sources, logs = native.load_document(
                body, self._fetch_stylesheets, fetch_js,
                page_url=final_url)
        finally:
            if prev is None:
                os.environ.pop("GGJS", None)
            else:
                os.environ["GGJS"] = prev

        self._doc = doc
        self._css_sources = css_sources
        self._injected = 0  # per-page injected-script budget (N1)
        self._console = list(logs)
        self._ver += 1
        self._export_cache = None
        self._styles_dirty = False
        if settle:
            self.settle()
        self.title = self._compute_title()
        return self

    # ---------- async event loop ----------

    def settle(self):
        """Drive the engine's event loop to quiescence: drain microtasks,
        fire virtual-clock timers, and service fetch() requests through the
        real network — looping until no work remains or the timeout. This
        is what makes fetch/setTimeout-driven SPA content materialize.
        No-op unless the gg-js async runtime is present."""
        if not (_HAS_PUMP and self.engine == "ggjs" and self._doc):
            return
        import time
        deadline = time.monotonic() + self.timeout
        mutated = False
        injected = getattr(self, "_injected", 0)
        for _ in range(_SETTLE_MAX_ROUNDS):
            logs, fetches = self._doc.pump()
            if logs:
                self._console.extend(logs)
                mutated = True
            if fetches:
                mutated = True
                for fetch_id, url in fetches:
                    self._service_fetch(fetch_id, url)
                continue  # resolves queued more microtasks; keep draining
            ran, injected = native._drain_injected_scripts(
                self._doc, self.url, injected)
            if ran:
                mutated = True
                continue  # injected code may queue more work
            if not self._doc.has_pending_work():
                break
            if time.monotonic() > deadline:
                self._console.append("[driver] settle timed out")
                break
        self._injected = injected
        if mutated:
            # a handler/timer may have changed the DOM; restyle + refresh
            self._doc.compute_styles(self._css_sources)
            self._ver += 1
            self._export_cache = None

    def _service_fetch(self, fetch_id, url):
        try:
            _h, body, _final = net.request_text(self.url.resolve(url))
            self._doc.resolve_fetch(fetch_id, 200, body)
        except Exception as e:
            self._doc.reject_fetch(fetch_id, f"{type(e).__name__}: {e}")

    def wait_for(self, target, timeout=None):
        """Settle the event loop, then test `target` (a selector string or
        a zero-arg predicate). Returns True if satisfied. With the virtual
        clock, settling drains all pending timers instantly, so this does
        not block on wall-clock timer delays."""
        saved = self.timeout
        if timeout is not None:
            self.timeout = timeout
        try:
            self.settle()
        finally:
            self.timeout = saved
        if callable(target):
            return bool(target())
        return self.exists(target)

    def _export(self):
        """Cached flat DOM export — full-tree marshal is the driver's
        biggest cost, so reuse it across reads until a mutation."""
        if self._export_cache is None or self._export_cache[0] != self._ver:
            self._export_cache = (self._ver, self._doc.export())
        return self._export_cache[1]

    def _run(self, sources):
        """Run scripts and invalidate the export cache (they may mutate)."""
        logs = self._doc.run_scripts(sources)
        self._console.extend(logs)
        self._ver += 1
        return logs

    def _fetch_stylesheets(self, hrefs):
        return self._fetch_many(hrefs)

    def _fetch_scripts(self, srcs):
        return self._fetch_many(srcs)

    def _fetch_many(self, urls):
        out = {}
        if not urls:
            return out

        def fetch(u):
            try:
                return net.request(self.url.resolve(u))[1]
            except Exception:
                return ""

        with ThreadPoolExecutor(max_workers=6) as pool:
            for u, text in zip(urls, pool.map(fetch, urls)):
                out.setdefault(u, text)
        return out

    # ---------- queries ----------

    def query(self, selector):
        """First element matching `selector`, or None."""
        hits = self._resolve(selector, first=True)
        return hits[0] if hits else None

    def query_all(self, selector):
        """All elements matching `selector`, in document order."""
        return self._resolve(selector, first=False)

    def text(self, selector):
        """textContent of the first match (engine semantics), or None."""
        el = self.query(selector)
        if el is None:
            return None
        return self.evaluate(
            "(function(){var e=document.querySelector(%s);"
            "return e?e.textContent:null;})()" % json.dumps(selector))

    def attr(self, selector, name):
        el = self.query(selector)
        return el.attr(name) if el else None

    def exists(self, selector):
        return self.query(selector) is not None

    # ---------- extraction (a Python-side taste of the P2 snapshot) ----------

    def links(self):
        """Every <a href> as Element handles."""
        return self.query_all("a")

    def snapshot(self):
        """Compact semantic projection of the page (roles/names/handles)
        an agent can consume without the full DOM dump — the AI-native
        primitive. Uses the Rust snapshot() when the wheel provides it
        (no whole-tree marshal), else falls back to a Python pass."""
        if _HAS_NATIVE:
            return [
                {"role": role, "tag": tag, "name": name, "ridx": ridx,
                 "href": href, "type": itype, "id": nid,
                 "interactive": interactive}
                for (ridx, role, tag, name, href, itype, nid, interactive)
                in self._doc.snapshot()
            ]
        flat = self._export()
        out = []
        for ei, (parent_idx, ridx, tag, text, attrs, _style) in \
                enumerate(flat):
            if tag is None:
                continue
            d = dict(attrs)
            role = _implicit_role(tag, d)
            if role is None:
                continue
            name = _subtree_text(flat, ei)
            out.append({
                "role": role,
                "tag": tag,
                "name": name.strip()[:120],
                "ridx": ridx,
                "href": d.get("href"),
                "type": d.get("type"),
                "id": d.get("id"),
                "interactive": tag in _INTERACTIVE,
            })
        return out

    # ---------- actions ----------

    def click(self, selector):
        """Click the first match. Returns True if a handler ran or the
        default (link navigation) should proceed."""
        el = self.query(selector)
        if el is None:
            return False
        return self._click_element(el)

    def _click_element(self, el):
        result = self._doc.dispatch_click(el._ridx)
        if len(result) == 3:
            logs, handled, prevented = result
        else:  # older wheel
            logs, handled = result
            prevented = handled
        self._console.extend(logs)
        self._ver += 1  # dispatch may have mutated the DOM
        # only re-style if a handler actually ran (a plain link/no-op
        # click cannot have changed the DOM, so skip the restyle cost)
        if handled:
            # a handler may have scheduled async work (fetch/setTimeout)
            self.settle()
            self._doc.compute_styles(self._css_sources)
        href = el.href  # already resolved on the Element — no extra export
        # navigate if it was a link and nothing prevented the default
        if not prevented and href \
                and not href.startswith(("javascript:", "mailto:", "#")):
            try:
                self.goto(self.url.resolve(href))
            except Exception:
                pass
        return handled or bool(href)

    # ---------- scripting ----------

    def evaluate(self, expression):
        """Run a JS *expression*, return its value decoded from JSON.
        Returns None if the engine cannot run it (see .console())."""
        code = "console.log(%s + JSON.stringify(%s))" % (
            json.dumps(_VAL_TAG), expression)
        logs = self._run([code])
        for line in logs:
            if line.startswith(_VAL_TAG):
                raw = line[len(_VAL_TAG):]
                try:
                    return json.loads(raw)
                except json.JSONDecodeError:
                    return raw
        return None

    def run(self, source):
        """Run arbitrary JS (statements). Returns console output lines."""
        return self._run([source])

    def console(self):
        return list(self._console)

    # ---------- internals ----------

    def _resolve(self, selector, first):
        """Resolve a selector to Element handles via the engine's own
        matcher. Native path (newer wheels) returns matches directly with
        no whole-tree marshal; else a marker-tagging fallback."""
        if _HAS_NATIVE:
            try:
                rows = self._doc.query(selector, first)
            except Exception:
                return []
            return [Element(self, ridx, tag, text, dict(attrs))
                    for (ridx, tag, attrs, text) in rows]
        token = "h%d" % (len(self._console) + id(selector) % 100000)
        limit = "1" if first else "__m.length"
        js = (
            "var __m = document.querySelectorAll(%s);"
            "var __n = %s;"
            "for (var i = 0; i < __n && i < __m.length; i++) {"
            "  __m[i].setAttribute(%s, %s + ':' + i); }"
        ) % (json.dumps(selector), limit, json.dumps(_MARKER),
             json.dumps(token))
        logs = self._run([js])
        if any(_is_error(l) for l in logs):
            return []

        flat = self._export()
        hits = []
        for ei, (parent_idx, ridx, tag, text, attrs, _style) in \
                enumerate(flat):
            mark = dict(attrs).get(_MARKER, "")
            if mark.startswith(token + ":"):
                try:
                    order = int(mark.split(":", 1)[1])
                except ValueError:
                    continue
                hits.append((order, Element(
                    self, ridx, tag, _subtree_text(flat, ei), dict(attrs))))
        hits.sort(key=lambda h: h[0])
        return [el for _, el in hits]

    def _compute_title(self):
        flat = self._export()
        for ei, row in enumerate(flat):
            if row[2] == "title":
                return _subtree_text(flat, ei).strip() or None
        return None


def _subtree_text(flat, target_ei):
    """Concatenated descendant text of the node at export index
    target_ei. export() is pre-order with parent-in-export-order links,
    so a forward pass gathers the whole subtree."""
    in_subtree = {target_ei}
    parts = []
    for ei in range(target_ei, len(flat)):
        parent = flat[ei][0]
        if ei == target_ei or parent in in_subtree:
            in_subtree.add(ei)
            if flat[ei][2] is None:  # text node: tag is None
                parts.append(flat[ei][3] or "")
    return "".join(parts)


_INTERACTIVE = {"a", "button", "input", "select", "textarea", "option"}
_HEADINGS = {"h1", "h2", "h3", "h4", "h5", "h6"}


def _implicit_role(tag, attrs):
    if "role" in attrs:
        return attrs["role"]
    if tag == "a" and "href" in attrs:
        return "link"
    if tag == "button":
        return "button"
    if tag == "input":
        return {"checkbox": "checkbox", "radio": "radio",
                "submit": "button", "button": "button"}.get(
            attrs.get("type", "text"), "textbox")
    if tag in ("select",):
        return "combobox"
    if tag == "textarea":
        return "textbox"
    if tag in _HEADINGS:
        return "heading"
    if tag == "nav":
        return "navigation"
    return None


def _is_error(line):
    return (line.startswith("[gg-js error]") or line.startswith("Uncaught")
            or line.startswith("ERROR"))
