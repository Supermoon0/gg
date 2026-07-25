"""Headless automation driver — GG as an embeddable, agent-drivable
engine (no GUI window, no tkinter). It can use either the local renderer or
the crash-contained renderer process without changing the Page API.

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

from . import forms, frame_bridge as _bridge, frames as _frames_mod, native, net
from .html_parser import tree_to_list
from .network_backend import default_network_backend
from .renderer_session import create_renderer_session

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

    def __init__(self, run_scripts=True, timeout=15,
                 network_backend=None, process_model=None):
        if not native.available():
            raise DriverError(
                "native ggcore wheel not installed — the driver needs it")
        self.run_scripts = run_scripts
        self.timeout = timeout
        self._network = network_backend or default_network_backend()
        self._renderer = create_renderer_session(
            self._network, process_model=process_model,
            run_scripts=run_scripts, timeout=timeout)
        self.url = None
        self.title = None
        self._doc = None
        self._css_sources = []
        self._console = []
        self._styles_dirty = False   # a handler mutated the DOM
        self._export_cache = None     # (version, flat); invalidated on mutation
        self._form_defaults = {}
        self._ver = 0
        self._navigation_token = None
        self._frames = None

    # ---------- navigation ----------

    def goto(self, url_or_str, settle=True, *, method="GET", body=None,
             headers=None):
        """Fetch, parse, run page scripts, compute styles. When `settle`
        (default), also run the event loop to a fixed point so async
        content (fetch/setTimeout) is present before you read the page.
        Returns self."""
        url = url_or_str if isinstance(url_or_str, net.URL) \
            else net.URL(url_or_str)
        self.cancel()
        if self._frames is not None:
            self._frames.dispose()
            self._frames = None
        self._renderer.close()
        token = self._network.new_cancel_token()
        self._navigation_token = token
        initiator = self.url
        if initiator is not None and not getattr(initiator, "host", None):
            initiator = None
        try:
            _headers, body, final_url = self._network.request_text(
                url, no_cache=True, method=method, body=body,
                headers=headers, site_for_cookies=initiator,
                top_level_navigation=True, timeout=self.timeout,
                cancel_token=token)
        except Exception:
            if self._navigation_token is token:
                self._navigation_token = None
            raise
        token.check()
        self.url = final_url

        try:
            root, doc, css_sources, logs = self._renderer.commit(
                self.url, body, cancel_token=token)
        except Exception:
            if self._navigation_token is token:
                self._navigation_token = None
            raise

        self._doc = doc
        self._css_sources = css_sources
        self._console = list(logs)
        self._form_defaults = forms.capture_defaults(root)
        self._ver += 1
        self._export_cache = None
        self._styles_dirty = False
        if settle:
            self.settle()
        self.title = self._compute_title()
        if self._navigation_token is token:
            self._navigation_token = None
        return self

    def cancel(self):
        """Cancel the current goto/resource load from another thread."""
        token = self._navigation_token
        if token is None:
            return False
        self._navigation_token = None
        return self._network.cancel(token)

    # ---------- async event loop ----------

    def settle(self):
        """Drive the engine's event loop to quiescence: drain microtasks,
        fire virtual-clock timers, and service fetch() requests through the
        real network — looping until no work remains or the timeout. This
        is what makes fetch/setTimeout-driven SPA content materialize.
        No-op unless the gg-js async runtime is present."""
        if not (_HAS_PUMP and self._doc):
            return
        update = self._renderer.settle(
            timeout=self.timeout, max_rounds=_SETTLE_MAX_ROUNDS,
            refresh=False)
        self._console.extend(update.logs)
        if update.dom_changed:
            # a handler/timer may have changed the DOM; restyle + refresh
            self._renderer.refresh()
            self._ver += 1
            self._export_cache = None
        self.pump_frames()

    def pump_frames(self, rounds=4):
        """Advance child frames and route cross-document messages.

        The shells do this on their live tick; headless assertions need
        the same, or a frame's timers and any postMessage handshake
        stay invisible to the test."""
        if self._frames is None:
            return False
        moved = False
        for _ in range(rounds):
            ticked = self._frames.tick(16.0)
            messaged = self._frames.pump_bridge()
            moved = moved or ticked or messaged
            if not (ticked or messaged):
                break
        return moved

    def _service_fetch(self, request):
        return self._renderer.service_fetch(request)

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
            self._export_cache = (self._ver, self._renderer.export())
        return self._export_cache[1]

    def _run(self, sources):
        """Run scripts and invalidate the export cache (they may mutate)."""
        logs = self._renderer.run(sources)
        self._console.extend(logs)
        self._ver += 1
        return logs

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
                in self._renderer.snapshot()
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
        result = self._renderer.dispatch_click(el._ridx)
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
            self._renderer.frame()
        href = el.href  # already resolved on the Element — no extra export
        activated = None
        # navigate if it was a link and nothing prevented the default
        if not prevented and href \
                and not href.startswith(("javascript:", "mailto:", "#")):
            try:
                self.goto(self.url.resolve(href))
            except Exception:
                pass
        elif not prevented:
            activated = self._form_activation(el._ridx)
            if activated is not None:
                self._console.extend(activated.logs)
            if activated is not None and (
                    activated.changed
                    or (activated.handled
                        and activated.submission is None)):
                self.settle()
                self._renderer.frame()
                self._ver += 1
                self._export_cache = None
            submission = activated.submission if activated else None
            if submission is not None:
                try:
                    self.goto(
                        self.url.resolve(submission.target),
                        method=submission.method, body=submission.body,
                        headers=submission.headers)
                except Exception:
                    pass
        return handled or bool(href) or activated is not None

    def _form_activation(self, ridx):
        root = native.build_tree(self._renderer.export())
        target = next((node for node in tree_to_list(root, [])
                       if getattr(node, "_ridx", None) == ridx), None)
        if target is None:
            return None
        return forms.activate_control(
            target, self.url, self._form_defaults,
            dispatch_event=self._renderer.dispatch_event,
            refresh_tree=lambda: native.build_tree(self._renderer.export()),
            set_attr=self._renderer.set_attr,
            remove_attr=self._renderer.remove_attr)

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

    # ---------- accessibility ----------

    def ax_tree(self):
        """The page's hierarchical accessibility tree as nested dicts
        (role/name/description/states/children, focusable/focused) —
        the full projection; snapshot() stays the flat fast path."""
        from . import accessibility
        root = native.build_tree(self._export())
        return accessibility.build_tree(root).to_dict()

    # ---------- frames ----------

    def frame(self, selector):
        """A FramePage handle into the first <iframe> matching
        `selector`, loading the frame's document on demand. The child
        runs fully isolated (own URL/origin/cookies/JS); this handle is
        the driver-level equivalent of contentDocument. Returns None
        when nothing matches or the frame could not load."""
        el = self.query(selector)
        if el is None or el.tag != "iframe":
            return None
        if self._frames is None:
            self._contexts = _bridge.ContextTable()
            self._contexts.register_top(
                self._renderer, url_getter=lambda: self.url)
            self._frames = _frames_mod.FrameManager(
                self._network, top_url=self.url, timeout=self.timeout,
                run_scripts=self.run_scripts,
                dispatch_event=lambda ridx, event_type:
                    self._renderer.dispatch_event(
                        ridx, event_type, False, False, None),
                contexts=self._contexts, session=self._renderer,
                parent_token=self._contexts.top)
        tree = native.build_tree(self._renderer.export())
        self._frames.sync(tree, self.url)
        for node in tree_to_list(tree, []):
            if getattr(node, "_ridx", None) == el._ridx:
                fd = getattr(node, "_frame", None)
                return FramePage(fd) if fd is not None else None
        return None

    def close(self):
        if self._frames is not None:
            self._frames.dispose()
            self._frames = None
        shutdown = getattr(self._renderer, "shutdown", None)
        if shutdown is not None:
            shutdown()
        else:
            self._renderer.close()

    # ---------- internals ----------

    def _resolve(self, selector, first):
        """Resolve a selector to Element handles via the engine's own
        matcher. Native path (newer wheels) returns matches directly with
        no whole-tree marshal; else a marker-tagging fallback."""
        if _HAS_NATIVE:
            try:
                rows = self._renderer.query(selector, first)
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


class FramePage:
    """Driver access into one iframe's isolated child document."""

    def __init__(self, fd):
        self._fd = fd

    @property
    def url(self):
        return self._fd.url

    @property
    def status(self):
        return self._fd.status  # loaded | blocked | error | empty

    @property
    def blocked_reason(self):
        return self._fd.blocked_reason

    def console(self):
        return list(self._fd.console)

    def post_message(self, data, target_origin="*"):
        """Deliver a message into this frame as the embedder would."""
        session = self._fd.session
        if session is None:
            return []
        return session.deliver_message(
            json.dumps(data), target_origin, 0)

    def navigate(self, href):
        """Follow a link inside the frame, recording it in the page's
        session history the way a click would."""
        return self._fd.navigate(href)

    def query_all(self, selector):
        session = self._fd.session
        if session is None:
            return []
        try:
            rows = session.query(selector, False)
        except Exception:
            return []
        return [Element(self, ridx, tag, text, dict(attrs))
                for (ridx, tag, attrs, text) in rows]

    def query(self, selector):
        session = self._fd.session
        if session is None:
            return None
        try:
            rows = session.query(selector, True)
        except Exception:
            return None
        for (ridx, tag, attrs, text) in rows:
            return Element(self, ridx, tag, text, dict(attrs))
        return None

    def exists(self, selector):
        return self.query(selector) is not None

    def evaluate(self, expression):
        session = self._fd.session
        if session is None:
            return None
        code = "console.log(%s + JSON.stringify(%s))" % (
            json.dumps(_VAL_TAG), expression)
        for line in session.run([code]):
            if line.startswith(_VAL_TAG):
                raw = line[len(_VAL_TAG):]
                try:
                    return json.loads(raw)
                except json.JSONDecodeError:
                    return raw
        return None

    def text(self, selector):
        el = self.query(selector)
        if el is None:
            return None
        return self.evaluate(
            "(function(){var e=document.querySelector(%s);"
            "return e?e.textContent:null;})()" % json.dumps(selector))

    def _click_element(self, el):
        """Frame-internal click: JS dispatch, then default link
        navigation stays inside the frame."""
        fd = self._fd
        handled = prevented = False
        if fd.session is not None:
            try:
                result = fd.session.dispatch_click(el._ridx)
                logs, handled, prevented = (
                    result if len(result) == 3
                    else (result[0], result[1], result[1]))
                fd.console.extend(logs)
                if handled:
                    fd.root = fd.session.frame()
                    fd._version += 1
                    fd._layout_cache = None
            except Exception:
                pass
        href = el.href
        if href and not prevented \
                and not href.startswith(("javascript:", "mailto:", "#")):
            fd.navigate(href)
            return True
        return handled or bool(href)

    def click(self, selector):
        el = self.query(selector)
        if el is None:
            return False
        return self._click_element(el)


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
