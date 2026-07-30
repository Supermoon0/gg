"""iframe: isolated child browsing contexts.

Each <iframe> gets its own FrameDocument — an isolated document with
its own URL/base URL, origin, cookie scope, storage, style sources,
JS event loop (a private gg-js Doc via LocalRendererSession), and its
own animation engine. The parent embeds only the child's *painted
output*: there is no shared DOM arena, so cross-origin scripting is
structurally impossible; the parent-child contract is limited to the
lifecycle events (load/error) the manager dispatches on the <iframe>
element and the frame's rendered box.

Embedding pipeline:

- FrameManager.sync() walks the parent tree, fetches new/changed
  frame documents (X-Frame-Options / CSP frame-ancestors enforced,
  sandbox applied), commits them into per-frame sessions, and
  dispatches load/error on the owning element.
- FrameManager.attach() re-points node._frame after every parent
  tree rebuild, hides the fallback content (an iframe's children
  never render), and gives the box its replaced-element default
  size (300x150, width/height attributes, CSS wins).
- BlockLayout.paint() sees node._frame and asks paint_cmds() for the
  child's display list, laid out at the frame's content size,
  scrolled, translated into parent document coordinates, and clipped
  to the frame box.
- The shells route hit-testing, link clicks, and wheel scrolling into
  the frame via the helpers at the bottom, and pump child event loops
  from their live tick (FrameManager.tick).

Depth and frame-count limits keep hostile nesting bounded.
"""

import time

from . import native, net
from .css_parser import CSSParser
from .draw import DrawClipPop, DrawClipPush, DrawLine, DrawRect, \
    DrawStickyPop, DrawStickyPush, translate_cmds
from .html_parser import Element, HTMLParser, Text, tree_to_list
from .network_backend import default_network_backend
from .style import RuleIndex, cascade_priority, default_rules, style
from . import animation
from . import frame_bridge

MAX_DEPTH = 3          # top-level document is depth 0
MAX_FRAMES_TOTAL = 16  # across the whole frame tree
DEFAULT_W = 300.0      # CSS replaced-element defaults for <iframe>
DEFAULT_H = 150.0
FRAME_SCROLL_STEP = 60


# ---------------------------------------------------------------------
# origin model
# ---------------------------------------------------------------------


def origin_of(url):
    """Schemeful origin tuple (scheme, host, port), or None for
    opaque/originless URLs (data:, about:, file:)."""
    if url is None:
        return None
    scheme = getattr(url, "scheme", None)
    host = getattr(url, "host", None)
    port = getattr(url, "port", None)
    if not scheme or not host:
        return None
    return (scheme, host, port)


def same_origin(a, b):
    """Schemeful same-origin check; opaque origins match nothing."""
    oa, ob = origin_of(a), origin_of(b)
    return oa is not None and oa == ob


def parse_sandbox(value):
    """The sandbox attribute -> None (absent: unrestricted) or the
    lowercase token set (possibly empty: fully sandboxed)."""
    if value is None:
        return None
    return {t.casefold() for t in value.split()}


def _origin_string(url):
    o = origin_of(url)
    if o is None:
        return None
    scheme, host, port = o
    default = {"http": 80, "https": 443}.get(scheme)
    if port and port != default:
        return f"{scheme}://{host}:{port}"
    return f"{scheme}://{host}"


def _ancestor_token_allows(token, top_url):
    """One CSP frame-ancestors source token vs the embedding origin."""
    t = token.strip().casefold()
    if not t or t == "'none'":
        return False
    if t == "*":
        return True
    top = _origin_string(top_url)
    if top is None:
        return False
    if t == top.casefold():
        return True
    # host-source without scheme, and *.example.com wildcards
    host = getattr(top_url, "host", "") or ""
    bare = t.split("://", 1)[-1]
    if bare.startswith("*."):
        return host.casefold().endswith(bare[1:])
    return bare == host.casefold()


def frame_blocked_reason(headers, child_url, top_url):
    """X-Frame-Options / CSP frame-ancestors (minimal) -> reason string
    when this response refuses to be embedded under top_url, else None.
    Per spec, frame-ancestors overrides X-Frame-Options when present."""
    lower = {str(k).casefold(): str(v) for k, v in (headers or {}).items()}
    csp = lower.get("content-security-policy", "")
    for directive in csp.split(";"):
        parts = directive.split()
        if not parts or parts[0].casefold() != "frame-ancestors":
            continue
        sources = parts[1:]
        if any(s.casefold() == "'self'" and same_origin(child_url, top_url)
               for s in sources):
            return None
        if any(_ancestor_token_allows(s, top_url) for s in sources):
            return None
        return "CSP frame-ancestors"
    xfo = lower.get("x-frame-options", "").strip().casefold()
    if xfo == "deny":
        return "X-Frame-Options: DENY"
    if xfo == "sameorigin" and not same_origin(child_url, top_url):
        return "X-Frame-Options: SAMEORIGIN"
    return None


class _CookielessBackend:
    """Network backend wrapper for sandboxed frames without
    allow-same-origin: an opaque origin never sends, stores, or reads
    credentials/cookies."""

    def __init__(self, inner):
        self._inner = inner

    def _strip(self, kwargs):
        kwargs = dict(kwargs)
        kwargs["send_cookies"] = False
        kwargs["store_cookies"] = False
        return kwargs

    def request_text(self, url, **kwargs):
        return self._inner.request_text(url, **self._strip(kwargs))

    def request(self, url, **kwargs):
        return self._inner.request(url, **self._strip(kwargs))

    def request_raw(self, url, **kwargs):
        return self._inner.request_raw(url, **self._strip(kwargs))

    def cookies_for(self, url, **kwargs):
        return ""

    def set_cookie_from_js(self, url, value, **kwargs):
        return 0

    def perform_script_fetch(self, base_url, request, **kwargs):
        return self._inner.perform_script_fetch(
            base_url, request, **kwargs)

    def bind_context(self, document_url, **kwargs):
        return self._inner.bind_context(document_url, **kwargs)

    def drop_context(self, context):
        return self._inner.drop_context(context)

    def new_cancel_token(self):
        return self._inner.new_cancel_token()

    def cancel(self, token):
        return self._inner.cancel(token)


# ---------------------------------------------------------------------
# one embedded document
# ---------------------------------------------------------------------


class FrameDocument:
    """An isolated child document plus its render state."""

    def __init__(self, network, *, top_url, depth, timeout,
                 run_scripts=True, use_native=None, budget=None):
        self.network = network
        self.top_url = top_url
        self.depth = depth
        self.timeout = timeout
        self.run_scripts = run_scripts
        self.use_native = (native.available()
                           if use_native is None else use_native)
        self.budget = budget if budget is not None else [MAX_FRAMES_TOTAL]

        self.url = None            # the child document's own URL
        self.source_key = None     # identity of the current content
        self.sandbox = None
        self.status = "empty"      # empty|loaded|blocked|error
        self.blocked_reason = ""
        self.root = None
        self.session = None
        self.css_sources = []
        self.console = []
        self.animator = animation.AnimationEngine()
        self.subframes = None      # nested FrameManager
        self.scroll = 0.0
        self._version = 0
        self._layout_cache = None  # (w, h, version) -> document layout
        # postMessage routing: the tab's shared table, this frame's own
        # context handle, and its embedder's
        self.contexts = None
        self.parent_token = 0
        self.handle = 0
        # session history: this frame's positional path under the top
        # document, and the callback that records a navigation
        self.path = ()
        self.on_navigate = None
        self._img_by_src = {}      # this frame's decoded images

    # -- loading -------------------------------------------------------

    def load(self, src, srcdoc, base_url, *, cancel_token=None,
             window_name=None):
        """(Re)load the frame from a src URL or srcdoc markup."""
        self.dispose_session()
        self.scroll = 0.0
        self._version += 1
        self._layout_cache = None
        self.console = []
        sandboxed = self.sandbox is not None
        allow_scripts = (not sandboxed) or "allow-scripts" in self.sandbox
        allow_origin = (not sandboxed) or "allow-same-origin" in self.sandbox
        network = self.network if allow_origin \
            else _CookielessBackend(self.network)

        try:
            if srcdoc is not None:
                # srcdoc documents inherit the parent's URL as base and
                # (unless sandboxed away) the parent's origin
                self.url = base_url
                body = srcdoc
            elif not src or src.strip().casefold() in ("about:blank",):
                self.url = base_url
                body = ""
            else:
                child_url = base_url.resolve(src)
                headers, body, final_url = network.request_text(
                    child_url, site_for_cookies=self.top_url,
                    top_level_navigation=False, timeout=self.timeout,
                    cancel_token=cancel_token)
                self.url = final_url
                reason = frame_blocked_reason(
                    headers, final_url, self.top_url)
                if reason:
                    self.status = "blocked"
                    self.blocked_reason = reason
                    self.root = None
                    return False
        except net.RequestCancelled:
            raise
        except Exception as exc:
            self.status = "error"
            self.blocked_reason = f"{type(exc).__name__}: {exc}"
            self.root = None
            return False

        try:
            self._commit(body, network, allow_scripts,
                         cancel_token=cancel_token,
                         window_name=window_name)
        except net.RequestCancelled:
            raise
        except Exception as exc:
            self.status = "error"
            self.blocked_reason = f"{type(exc).__name__}: {exc}"
            self.root = None
            return False
        try:
            self.load_images()
        except net.RequestCancelled:
            raise
        except Exception:
            pass
        self.status = "loaded"
        self.blocked_reason = ""
        return True

    def _commit(self, body, network, allow_scripts, *, cancel_token=None,
                window_name=None):
        if self.use_native:
            from .renderer_session import LocalRendererSession
            self.session = LocalRendererSession(
                network, run_scripts=(self.run_scripts and allow_scripts),
                timeout=self.timeout)
            root, _doc, css_sources, logs = self.session.commit(
                self.url, body, cancel_token=cancel_token, framed=True,
                window_name=window_name)
            self.root = root
            self.css_sources = list(css_sources)
            self.console = list(logs)
        else:
            self.session = None
            self.root = HTMLParser(body).parse()
            rules = default_rules()
            texts = []
            for node in tree_to_list(self.root, []):
                if not isinstance(node, Element):
                    continue
                if node.tag == "style":
                    css = "".join(c.text for c in node.children
                                  if isinstance(c, Text))
                    texts.append(css)
                elif (node.tag == "link" and "href" in node.attributes
                      and node.attributes.get(
                          "rel", "").casefold() == "stylesheet"):
                    try:
                        _h, css, _f = network.request_text(
                            self.url.resolve(node.attributes["href"]),
                            site_for_cookies=self.top_url,
                            top_level_navigation=False,
                            timeout=self.timeout,
                            cancel_token=cancel_token)
                        texts.append(css)
                    except Exception:
                        pass
            for css in texts:
                rules.extend(CSSParser(css).parse())
            style(self.root, RuleIndex(sorted(rules, key=cascade_priority)))
            self.css_sources = texts
        self.animator.reset(self.css_sources)
        self.animator.on_frame(self.root)
        # join the tab's messaging graph (a fresh handle per load: the
        # old document is gone, and a stale handle must not resolve)
        if self.contexts is not None and self.session is not None:
            ctx = self.contexts.register(
                self.session, frame=self, parent=self.parent_token)
            ctx.console = self.console
            self.handle = ctx.token
        # nested browsing contexts, one level deeper
        if self.depth + 1 <= MAX_DEPTH:
            self.subframes = FrameManager(
                self.network, top_url=self.top_url, depth=self.depth + 1,
                timeout=self.timeout,
                run_scripts=(self.run_scripts and allow_scripts),
                use_native=self.use_native, budget=self.budget,
                dispatch_event=self._dispatch_child_event,
                contexts=self.contexts, session=self.session,
                parent_token=self.handle,
                on_navigate=self.on_navigate, path_prefix=self.path)
            self.subframes.sync(self.root, self.url,
                                cancel_token=cancel_token)
        self._version += 1
        self._layout_cache = None

    def _dispatch_child_event(self, ridx, event_type):
        if self.session is None:
            return
        try:
            logs, _h, _p = self.session.dispatch_event(
                ridx, event_type, False, False, None)
            self.console.extend(logs)
        except Exception:
            pass

    def navigate(self, href, *, record=True):
        """In-frame navigation (a link inside the frame was activated).

        A frame navigation is its own session-history entry, so the
        host is told *before* the load — it has to snapshot the state
        being left, not the one being entered. `record=False` is how
        back/forward replays a navigation without recording it again.
        """
        if self.url is None:
            return False
        target = self.url.resolve(href)
        if record and self.on_navigate is not None:
            try:
                self.on_navigate(self.path, str(target))
            except Exception:
                pass
        ok = self.load(str(target), None, self.url)
        self._version += 1
        self._layout_cache = None
        return ok

    # -- event loop ----------------------------------------------------

    def tick(self, dt_ms=None):
        """Advance the frame's JS clock + animations. Returns True when
        the frame needs repainting."""
        changed = False
        if self.session is not None:
            try:
                update = self.session.tick(dt_ms)
                if update.dom_changed:
                    self.root = self.session.frame()
                    self._version += 1
                    self._layout_cache = None
                    if self.subframes is not None:
                        self.subframes.sync(self.root, self.url)
                    self.animator.on_frame(self.root)
                    # new markup can carry new images -- an ad creative
                    # arrives this way, written in after the document
                    try:
                        self.load_images()
                    except Exception:
                        pass
                    changed = True
            except Exception:
                pass
        if self.root is not None:
            result = self.animator.on_frame(self.root)
            if result.damage != "none":
                self._version += 1
                self._layout_cache = None
                changed = True
        if self.subframes is not None and self.subframes.tick(dt_ms):
            changed = True
        return changed

    # -- layout / paint ------------------------------------------------

    def _document_layout(self, width, height):
        from .layout import DocumentLayout
        key = (round(width, 1), round(height, 1), self._version)
        if self._layout_cache and self._layout_cache[0] == key:
            return self._layout_cache[1]
        if self.root is None:
            return None
        doc = DocumentLayout(self.root)
        doc.layout(max(width, 10.0), max(height, 10.0))
        self._layout_cache = (key, doc)
        self._push_layout_rects(doc)
        return doc

    def load_images(self):
        """Decode this frame's own <img> sources.

        The shell only ever loaded the *top* document's images, so
        every image inside an iframe painted as a broken-image box —
        and an ad creative is nothing but one image inside a frame.
        Sources are cached per frame, so a re-render costs nothing and
        a document that swaps its creative fetches only the new one."""
        if self.root is None or self.url is None:
            return False
        from . import textengine
        if not textengine.available():
            return False
        nodes = [n for n in tree_to_list(self.root, [])
                 if isinstance(n, Element) and n.tag == "img"
                 and n.attributes.get("src")]
        if not nodes:
            return False
        engine = textengine.engine()
        loaded = False
        for src in {n.attributes["src"] for n in nodes}:
            if src in self._img_by_src:
                continue
            data = b""
            try:
                data = self.network.request_raw(
                    self.url.resolve(src), site_for_cookies=self.top_url,
                    top_level_navigation=False, timeout=self.timeout)[1]
            except net.RequestCancelled:
                raise
            except Exception:
                data = b""
            try:
                self._img_by_src[src] = textengine.load_image_data(data)
            except Exception:
                self._img_by_src[src] = None
            loaded = True
        for node in nodes:
            node._img = self._img_by_src.get(node.attributes["src"])
        if loaded:
            self._layout_cache = None
        return loaded

    def _push_layout_rects(self, doc):
        """Give the child's scripts their own real geometry.

        The top document has done this all along; a frame never did, so
        everything inside one read getBoundingClientRect as 0x0. A
        SafeFrame ad is built entirely on the opposite: the child
        observes its own body and posts the measured height out to the
        host, which is what finally gives the <iframe> a height."""
        if self.session is None:
            return
        rects, stack = [], [doc]
        while stack:
            b = stack.pop()
            node = getattr(b, "node", None)
            ridx = getattr(node, "_ridx", None) if node is not None else None
            if ridx is not None:
                rects.append((int(ridx), float(getattr(b, "x", 0.0)),
                              float(getattr(b, "y", 0.0)),
                              float(getattr(b, "width", 0.0)),
                              float(getattr(b, "height", 0.0))))
            stack.extend(getattr(b, "children", None) or [])
        try:
            self.session.set_layout_rects(rects)
        except Exception:
            pass

    def content_height(self, width, height):
        doc = self._document_layout(width, height)
        if doc is None:
            return 0.0
        from .layout import VSTEP, layout_tree_to_list
        return max((o.y + o.height
                    for o in layout_tree_to_list(doc, [])
                    if getattr(o, "height", None) is not None),
                   default=doc.height + 2 * VSTEP)

    def max_scroll(self, width, height):
        return max(0.0, self.content_height(width, height) - height)

    def scroll_by(self, delta, width, height):
        before = self.scroll
        self.scroll = min(max(0.0, self.scroll + delta),
                          self.max_scroll(width, height))
        return self.scroll != before

    def paint_cmds(self, x, y, width, height):
        """The frame's content as display commands in parent document
        coordinates, clipped to the frame box."""
        if self.status in ("blocked", "error"):
            return [
                DrawRect(x, y, x + width, y + height, "#f2f2f2"),
                DrawLine(x, y, x + width, y + height, "#bbbbbb"),
                DrawLine(x + width, y, x, y + height, "#bbbbbb"),
            ]
        doc = self._document_layout(width, height)
        if doc is None:
            return []
        from .layout import paint_tree
        sub = paint_tree(doc, [])
        # sticky offsets are resolved against the top-level scroll at
        # raster time; inside a frame they would track the wrong
        # scroller, so frames paint sticky content at its normal flow
        # position instead
        sub = [c for c in sub
               if not isinstance(c, (DrawStickyPush, DrawStickyPop))]
        self.scroll = min(self.scroll, self.max_scroll(width, height))
        translate_cmds(sub, x, y - self.scroll)
        return ([DrawClipPush(x, y, x + width, y + height)]
                + sub + [DrawClipPop()])

    # -- hit testing ----------------------------------------------------

    def hit_test(self, cx, cy, width, height):
        """Deepest layout object at frame-content coordinates (already
        scroll-adjusted by hit_child)."""
        doc = self._document_layout(width, height)
        if doc is None:
            return None
        from .layout import layout_tree_to_list
        objs = [o for o in layout_tree_to_list(doc, [])
                if o.x <= cx < o.x + o.width
                and o.y <= cy < o.y + o.height]
        return objs[-1] if objs else None

    def find_link(self, node):
        while node is not None:
            if (isinstance(node, Element) and node.tag == "a"
                    and "href" in node.attributes):
                return node.attributes["href"]
            node = node.parent
        return None

    def click_at(self, cx, cy, width, height):
        """Dispatch a click at frame-content coordinates: JS first,
        then the default link action (an in-frame navigation).
        Returns 'navigated' | 'handled' | None."""
        obj = self.hit_test(cx, cy, width, height)
        if obj is None:
            return None
        target = obj.node
        handled = prevented = False
        if self.session is not None:
            el = target
            while el is not None and getattr(el, "_ridx", None) is None:
                el = el.parent
            if el is not None:
                try:
                    result = self.session.dispatch_click(el._ridx)
                    logs, handled, prevented = (
                        result if len(result) == 3
                        else (result[0], result[1], result[1]))
                    self.console.extend(logs)
                    if handled:
                        self.root = self.session.frame()
                        self._version += 1
                        self._layout_cache = None
                        if self.subframes is not None:
                            self.subframes.sync(self.root, self.url)
                except Exception:
                    pass
        href = self.find_link(target)
        if href and not prevented \
                and not href.startswith(("javascript:", "mailto:", "#")):
            self.navigate(href)
            return "navigated"
        return "handled" if (handled or href) else None

    # -- teardown -------------------------------------------------------

    def _drop_context(self):
        if self.contexts is not None and self.handle:
            self.contexts.drop(self.handle)
        self.handle = 0

    def dispose_session(self):
        self._drop_context()
        if self.subframes is not None:
            self.subframes.dispose()
            self.subframes = None
        if self.session is not None:
            try:
                self.session.close()
            except Exception:
                pass
            self.session = None

    def dispose(self):
        self.dispose_session()
        self.root = None
        self._layout_cache = None


# ---------------------------------------------------------------------
# per-document frame registry
# ---------------------------------------------------------------------


def _console_line(ctx, line):
    """Handler output goes to the console of the document that ran it."""
    sink = getattr(ctx, "console", None)
    if sink is None and ctx.frame is not None:
        sink = ctx.frame.console
    if sink is not None:
        sink.append(line)


def _frame_key(node):
    ridx = getattr(node, "_ridx", None)
    return ("r", ridx) if ridx is not None else ("p", id(node))


def _source_key(node, written=None):
    """Identity of the content an iframe element asks for.

    A document a script wrote into the frame wins over the element's
    own attributes: `f.contentDocument.write(html)` replaces whatever
    `src` said, exactly as it does in a real browser."""
    ridx = getattr(node, "_ridx", None)
    if written and ridx is not None and ridx in written:
        return ("written", written[ridx], node.attributes.get("sandbox"))
    if "srcdoc" in node.attributes:
        return ("srcdoc", node.attributes.get("srcdoc", ""),
                node.attributes.get("sandbox"))
    return ("src", node.attributes.get("src", ""),
            node.attributes.get("sandbox"))


class FrameManager:
    """Owns every FrameDocument under one parent document."""

    def __init__(self, network=None, *, top_url=None, depth=0,
                 timeout=net.DEFAULT_TIMEOUT, run_scripts=True,
                 use_native=None, budget=None, dispatch_event=None,
                 contexts=None, session=None, parent_token=0,
                 on_navigate=None, path_prefix=()):
        self.network = network or default_network_backend()
        self.top_url = top_url
        self.depth = depth
        self.timeout = timeout
        self.run_scripts = run_scripts
        self.use_native = use_native
        self.budget = budget if budget is not None else [MAX_FRAMES_TOTAL]
        self.dispatch_event = dispatch_event
        # the tab's postMessage routing table, shared down the tree the
        # same way `budget` is. `session` is the *owning* document's
        # renderer session — the one that embeds these frames.
        self.contexts = contexts
        self.session = session
        self.parent_token = parent_token
        # session history: where this frame sits in document order at
        # each depth, and who to tell when it navigates
        self.on_navigate = on_navigate
        self.path_prefix = tuple(path_prefix)
        self.frames = {}
        # iframe node index -> markup a script wrote into it with
        # contentDocument.write(). Sticky: the frame keeps showing it
        # until another write replaces it.
        self.written = {}

    # -- discovery ------------------------------------------------------

    @staticmethod
    def _iframe_nodes(root):
        return [n for n in tree_to_list(root, [])
                if isinstance(n, Element) and n.tag == "iframe"]

    def sync(self, root, page_url, *, cancel_token=None):
        """Load new/changed frames, drop removed ones, re-attach all.
        Returns True when any frame content changed."""
        if self.top_url is None:
            self.top_url = page_url
        self._drain_document_writes()
        changed = False
        seen = {}
        for node in self._iframe_nodes(root):
            key = _frame_key(node)
            want = _source_key(node, self.written)
            fd = self.frames.get(key)
            if fd is not None and fd.source_key != want:
                fd.dispose()
                self.budget[0] += 1
                fd = None
            if fd is None:
                if self.depth >= MAX_DEPTH or self.budget[0] <= 0:
                    self._attach_one(node, None)
                    continue
                fd = FrameDocument(
                    self.network, top_url=self.top_url or page_url,
                    depth=self.depth, timeout=self.timeout,
                    run_scripts=self.run_scripts,
                    use_native=self.use_native, budget=self.budget)
                fd.sandbox = parse_sandbox(
                    node.attributes.get("sandbox"))
                fd.source_key = want
                fd.contexts = self.contexts
                fd.parent_token = self.parent_token
                self.budget[0] -= 1
                if want[0] == "written":
                    src, srcdoc = None, want[1]
                elif "srcdoc" in node.attributes:
                    src = node.attributes.get("src")
                    srcdoc = node.attributes.get("srcdoc")
                else:
                    src, srcdoc = node.attributes.get("src"), None
                ok = fd.load(src, srcdoc, page_url,
                             cancel_token=cancel_token,
                             window_name=node.attributes.get("name"))
                changed = True
                self.frames[key] = fd
                # contentWindow must resolve before `load` fires: the
                # canonical embed idiom is
                # `iframe.onload = () => iframe.contentWindow.postMessage(...)`
                self.frames = dict(self.frames)
                self.publish_frames(root, page_url)
                self._lifecycle(node, ok)
            fd.path = self.path_prefix + (len(seen),)
            fd.on_navigate = self.on_navigate
            seen[key] = fd
            self._attach_one(node, fd)
        for key, fd in self.frames.items():
            if key not in seen:
                fd.dispose()
                self.budget[0] += 1
                changed = True
        self.frames = seen
        self.publish_frames(root, page_url)
        return changed

    def _drain_document_writes(self):
        """Collect `contentDocument.write()` markup from the owning
        document. The VM has no child arena, so it buffers the markup
        and the frame machinery here turns it into a real document."""
        if self.session is None:
            return
        try:
            writes = self.session.take_document_writes()
        except Exception:
            return           # older wheel without the seam
        for node_idx, html in writes or []:
            self.written[int(node_idx)] = html

    def publish_frames(self, root, owner_url):
        """Tell the owning document which of its <iframe> elements map
        to which browsing context."""
        if self.session is None or root is None:
            return
        try:
            self.session.set_frame_graph(
                frame_bridge.frame_graph(self, root, owner_url))
        except Exception:
            return   # older wheel without the frame seam
        # the mirror has to exist before `load` fires: reading
        # `this.contentDocument` from an onload handler is the whole
        # point of the same-origin case
        if self.contexts is not None:
            frame_bridge.sync_mirrors(self.contexts)

    def ordered_frames(self):
        """The frames this document embeds, in document order — the
        order `path` ordinals are assigned in."""
        return list(self.frames.values())

    def frame_at(self, path):
        """The FrameDocument a history row's path names, or None.

        Paths are positional, so a page that reshuffled its frames
        resolves to a different frame or to nothing; that is the same
        bargain form-state restore already makes."""
        manager, fd = self, None
        for ordinal in path:
            if manager is None:
                return None
            kids = manager.ordered_frames()
            if ordinal >= len(kids):
                return None
            fd = kids[ordinal]
            manager = fd.subframes
        return fd

    def pump_bridge(self):
        """Route this turn's postMessage traffic across the whole tab.

        Driven from the shells' live tick rather than from `tick`, so a
        message is not starved on a turn where the frame tree happened
        not to need advancing."""
        if self.contexts is None:
            return False
        frame_bridge.sync_mirrors(self.contexts)
        messaged = frame_bridge.pump(self.contexts, log=_console_line)
        wrote = frame_bridge.pump_dom_writes(
            self.contexts, log=_console_line)
        if wrote:
            self._refresh_written_frames()
        return messaged or wrote

    def _refresh_written_frames(self):
        """A parent write landed in a child: re-read its tree so the
        next paint shows it."""
        for fd in self.frames.values():
            if fd.session is None:
                continue
            try:
                fd.root = fd.session.frame()
            except Exception:
                continue
            fd._version += 1
            fd._layout_cache = None
            if fd.subframes is not None:
                fd.subframes._refresh_written_frames()

    def _lifecycle(self, node, ok):
        """Fire load/error on the owning <iframe> element."""
        if self.dispatch_event is None:
            return
        ridx = getattr(node, "_ridx", None)
        if ridx is None:
            return
        try:
            self.dispatch_event(ridx, "load" if ok else "error")
        except Exception:
            pass

    def attach(self, root):
        """After a parent tree rebuild: re-point node._frame, hide the
        fallback content, default the replaced box size."""
        for node in self._iframe_nodes(root):
            self._attach_one(node, self.frames.get(_frame_key(node)))

    @staticmethod
    def _attach_one(node, fd):
        node._frame = fd
        # an iframe's children are fallback content — never rendered
        node.children = []
        style_map = node.style
        if style_map.get("display", "inline") in ("", "inline"):
            style_map["display"] = "inline-block"

        def default_size(prop, fallback):
            if style_map.get(prop):
                return
            attr = (node.attributes.get(prop) or "").strip()
            if attr.endswith("%"):
                style_map[prop] = attr
                return
            try:
                style_map[prop] = f"{float(attr.replace('px', ''))}px"
            except ValueError:
                style_map[prop] = fallback

        default_size("width", f"{DEFAULT_W}px")
        default_size("height", f"{DEFAULT_H}px")

    # -- event loop -----------------------------------------------------

    def tick(self, dt_ms=None):
        changed = False
        for fd in self.frames.values():
            if fd.tick(dt_ms):
                changed = True
        return changed

    def active(self):
        return bool(self.frames) and any(
            fd.session is not None or fd.animator.active
            for fd in self.frames.values())

    def animations_active(self):
        """Any frame (at any depth) wants animation-paced frames."""
        for fd in self.frames.values():
            if fd.animator.active:
                return True
            if fd.subframes is not None \
                    and fd.subframes.animations_active():
                return True
        return False

    def dispose(self):
        # refund the shared budget: without this a page that swaps its
        # frame tree repeatedly runs out of frames it is not using
        self.budget[0] += len(self.frames)
        for fd in self.frames.values():
            fd.dispose()
        self.frames = {}


# ---------------------------------------------------------------------
# shell helpers: routing input into frames
# ---------------------------------------------------------------------


def frame_of(obj):
    """The FrameDocument painted by this layout object, or None."""
    node = getattr(obj, "node", None)
    return getattr(node, "_frame", None) if node is not None else None


def child_coords(obj, doc_x, doc_y, frame):
    """Parent document coords -> frame content coords."""
    return (doc_x - obj.x, doc_y - obj.y + frame.scroll)


def hit_frame(obj, doc_x, doc_y):
    """When obj is an iframe box with content, resolve the hit into
    (frame, child_layout_object); else None."""
    frame = frame_of(obj)
    if frame is None or frame.root is None:
        return None
    cx, cy = child_coords(obj, doc_x, doc_y, frame)
    child = frame.hit_test(cx, cy, obj.width, obj.height)
    if child is None:
        return None
    return frame, child
