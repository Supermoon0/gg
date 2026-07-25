"""Native window shell: Rust winit window, all chrome drawn by us.

No tkinter anywhere: the Rust rasterizer draws the page AND the
browser UI (toolbar, URL bar, status bar) into one frame, which is
blitted straight onto the window surface via softbuffer.
"""

import os
import time
import traceback
from concurrent.futures import ThreadPoolExecutor

from . import (animation, forms, frame_bridge, frames, keyboard, native,
               navigation, net, textengine)
from .draw import scale_cmds
from .html_parser import Element, Text, tree_to_list
from .layout import (HSTEP, VSTEP, DocumentLayout, apply_scroll_requests,
                     capture_scroll_state, collect_scroll_state,
                     find_scrollable, get_font, hit_test_at,
                     layout_tree_to_list, measure, paint_tree,
                     restore_scroll_state, scroll_container_by)
from .pages import error_page
from .network_backend import (create_network_backend,
                              default_network_backend)
from .renderer_session import create_renderer_session

TOOLBAR_H = 44
STATUS_H = 24
SCROLL_STEP = 90
HOME_URL = "about:home"

TOOLBAR_BG = (232, 232, 232)
STATUS_BG = (240, 240, 240)
BORDER = (200, 200, 200)
INK = (40, 40, 40)


def _fetch_many(urls, base, binary=False, *, timeout=net.DEFAULT_TIMEOUT,
                cancel_token=None, network_backend=None,
                network_context=None):
    """Parallel fetch helper: {url: text-or-bytes}."""
    network_backend = network_backend or default_network_backend()

    def fetch(u):
        try:
            if binary:
                return network_backend.request_raw(
                    base.resolve(u), site_for_cookies=base,
                    top_level_navigation=False, timeout=timeout,
                    cancel_token=cancel_token,
                    context=network_context)[1]
            return network_backend.request(
                base.resolve(u), site_for_cookies=base,
                top_level_navigation=False, timeout=timeout,
                cancel_token=cancel_token,
                context=network_context)[1]
        except net.RequestCancelled:
            raise
        except Exception:
            return b"" if binary else ""

    out = {}
    if urls:
        with ThreadPoolExecutor(max_workers=6) as pool:
            for u, data in zip(urls, pool.map(fetch, urls)):
                out.setdefault(u, data)
    return out


class Shell:
    def __init__(self, win, network_backend=None, process_model=None):
        self.win = win
        self.network = network_backend or create_network_backend()
        model = process_model or os.environ.get(
            "GG_PROCESS_MODEL", "isolated")
        self.renderer = create_renderer_session(
            self.network, process_model=model,
            timeout=net.DEFAULT_TIMEOUT)
        self.engine = textengine.engine()
        self.running = True
        self.dirty = True
        self.scroll = 0
        self.hscroll = 0
        self.content_width = 0
        self.content_height = 0
        self._styled_width = 0.0
        self.url = None
        self.url_text = ""
        self.caret = 0
        self.url_focused = False
        self.history = []
        self.history_index = -1
        self.navigation_timeout = net.DEFAULT_TIMEOUT
        self._navigation = navigation.NavigationController(
            network_backend=self.network)
        self._loading_token = None
        self.document = None
        self.layout_list = []
        self.display_list = []
        self._list_dirty = True
        self._pushed_scale = None
        self.nodes = None
        self._doc = None
        self.focus_node = None
        self._focus_ridx = None
        self._css_sources = []
        self._form_defaults = {}
        self._img_by_src = {}
        self._load_timings = {}
        self._deferred_resources = False
        self._dom_version = None
        self._live_last = time.monotonic()
        self._live_next = 0.0
        self.animator = animation.AnimationEngine()
        self.frames = None
        # frame session history: the state a back/forward is replaying,
        # and the guard that stops the replay recording itself
        self._frames_pending = ()
        self._frames_restoring = False
        self._scroll_state = {}
        self._mouse_pos = (0, 0)
        self.status = ""
        self.ui_font = get_font(15, "normal", "roman", "default")
        self.ui_small = get_font(12, "normal", "roman", "default")
        self._last_motion = 0.0
        self._buttons = []
        self._urlbar = (0, 0, 0, 0)

    # ---------- loading ----------

    def load_url_string(self, text):
        text = text.strip()
        if not text:
            return
        if "://" not in text and not text.startswith(("about:", "data:")):
            text = "https://" + text
        try:
            url = net.URL(text)
        except Exception:
            url = net.URL(HOME_URL)
        self.load(url)

    def load(self, url, add_to_history=True, no_cache=False, *,
             method="GET", body=None, headers=None, timeout=None):
        self._snapshot_history_entry()
        self.set_status(f"로딩 중... {url}")
        initiator = getattr(self, "url", None)
        if initiator is not None and not getattr(initiator, "host", None):
            initiator = None
        timeout = self.navigation_timeout if timeout is None else timeout
        context = {"url": url, "add_to_history": add_to_history}

        def fetch(token):
            return self.network.request_text(
                url, no_cache=no_cache, method=method, body=body,
                headers=headers, site_for_cookies=initiator,
                top_level_navigation=True, timeout=timeout,
                cancel_token=token)

        return self._navigation.start(fetch, context).future

    def poll_navigation(self):
        pending = self._navigation.take_ready()
        if pending is None:
            return False
        context = pending.context
        url = context["url"]
        try:
            _headers, page_body, url = pending.future.result()
        except net.RequestCancelled:
            self.set_status("탐색 취소됨")
            return True
        except Exception as exc:
            page_body = error_page(
                str(url), f"{type(exc).__name__}: {exc}")
        self._loading_token = pending.token
        try:
            self.render_page(url, page_body)
        except Exception:
            page_body = error_page(str(url), traceback.format_exc(limit=5))
            self.render_page(url, page_body)
        finally:
            self._loading_token = None
        if context["add_to_history"]:
            self.history = self.history[:self.history_index + 1]
            self.history.append(navigation.HistoryEntry(url, page_body))
            self.history_index = len(self.history) - 1
        elif 0 <= self.history_index < len(self.history):
            self.history[self.history_index] = navigation.HistoryEntry(
                url, page_body)
        self.set_status(
            "첫 화면 · 나머지 로딩 중" if self._deferred_resources
            else "완료 (native window)")
        return True

    def cancel_navigation(self):
        if self._navigation.cancel():
            self.set_status("탐색 취소됨")

    def _snapshot_history_entry(self):
        if not (0 <= self.history_index < len(self.history)):
            return
        entry = self.history[self.history_index]
        entry.scroll = float(self.scroll)
        entry.hscroll = float(self.hscroll)
        entry.form_state = navigation.capture_form_state(self.nodes)
        entry.frame_state = navigation.capture_frame_state(self.frames)

    def _on_frame_navigated(self, path, url):
        """A link inside a frame was followed: that is a session-history
        entry of its own, so back returns the frame rather than
        reloading the whole page."""
        if self._frames_restoring:
            return
        if not (0 <= self.history_index < len(self.history)):
            return
        self._snapshot_history_entry()
        current = self.history[self.history_index]
        self.history = self.history[:self.history_index + 1]
        # same document, new frame position: reuse the url and body so
        # going back is a frame navigation, not a refetch
        self.history.append(navigation.HistoryEntry(
            current.url, current.body, scroll=current.scroll,
            hscroll=current.hscroll, form_state=current.form_state,
            frame_state=navigation.frame_state_with(
                current.frame_state, path, url)))
        self.history_index = len(self.history) - 1

    def _restore_history_entry(self, entry, previous=None):
        if previous is not None and entry.url is previous.url \
                and entry.body is previous.body:
            # a frame-only entry: re-pointing the frames is the whole
            # restore, and re-rendering the page would throw them away
            self._frames_restoring = True
            try:
                navigation.restore_frame_state(self.frames, entry.frame_state)
            finally:
                self._frames_restoring = False
            self.relayout()
            self.scroll = entry.scroll
            self.hscroll = entry.hscroll
            self.clamp_scroll()
            self.set_status("완료 (history 복원)")
            self.dirty = True
            return
        self._frames_pending = entry.frame_state
        self.render_page(entry.url, entry.body)
        changed = navigation.restore_form_state(
            self.nodes, entry.form_state,
            set_attr=self.renderer.set_attr,
            remove_attr=self.renderer.remove_attr)
        if changed:
            self.nodes = self.renderer.frame(self._styled_width)
            self.relayout()
        self.scroll = entry.scroll
        self.hscroll = entry.hscroll
        self.clamp_scroll()
        self.set_status("완료 (history 복원)")
        self.dirty = True

    def render_page(self, url, body):
        render_started = time.perf_counter()
        self._load_timings = {}
        self.url = url
        self.url_text = str(url)
        self.caret = len(self.url_text)
        self.url_focused = False
        self.focus_node = None
        self._focus_ridx = None

        self._styled_width = self.logical_size()[0]
        load_timings = {}
        self.renderer.timeout = self.navigation_timeout
        self.nodes, self._doc, self._css_sources, logs = \
            self.renderer.commit(
                url, body, viewport_width=self._styled_width,
                timings=load_timings,
                cancel_token=self._loading_token)
        for line in logs:
            print(f"[js console] {line}")

        self._load_timings = load_timings
        self._form_defaults = forms.capture_defaults(self.nodes)

        self.apply_title()
        if self.frames is not None:
            self.frames.dispose()
        self._contexts = frame_bridge.ContextTable()
        self._contexts.register_top(
            self.renderer, url_getter=lambda: self.url)
        self.frames = frames.FrameManager(
            self.network, top_url=url, timeout=self.navigation_timeout,
            dispatch_event=self._dispatch_frame_event,
            contexts=self._contexts, session=self.renderer,
            parent_token=self._contexts.top,
            on_navigate=self._on_frame_navigated)
        self._scroll_state = {}
        self.animator.reset(self._css_sources)
        # Paint the DOM committed by parser-time scripts first. Async data,
        # images and lazy cards continue from tick_live after this frame.
        self.engine.clear_images()
        self._img_by_src = {}
        self._deferred_resources = True
        self.scroll = 0
        self.hscroll = 0
        self.relayout()
        self._dom_version = self.renderer.state().dom_version
        self._live_last = time.monotonic()
        self._live_next = self._live_last
        self._load_timings["first_paint"] = \
            (time.perf_counter() - render_started) * 1000.0
        summary = " ".join(
            f"{name}={value:.1f}ms"
            for name, value in self._load_timings.items())
        print(f"[perf] {url} {summary}")

    def tick_live(self):
        """Advance one page turn after the current frame was presented."""
        if self._doc is None:
            return
        now = time.monotonic()
        if now < self._live_next:
            return
        dt = min((now - self._live_last) * 1000.0, 1000.0)
        self._live_last = now
        self._live_next = now + 0.08
        changed = False
        try:
            if hasattr(self._doc, "tick") and native.async_available():
                update = self.renderer.tick(dt)
                for line in update.logs:
                    print(f"[js live] {line}")
                changed = update.dom_changed
                if changed:
                    self.nodes = self.renderer.frame(self._styled_width)
                    self._remap_focus()
            # scrolling the page scripts asked for this turn must be
            # applied on EVERY tick — a tick that also rebuilt the DOM
            # or loaded deferred resources would otherwise drop the
            # request and then push the stale offset back over it
            scrolled = self._apply_scroll_writes()
            # cross-document messages route on every tick too, for the
            # same reason: a busy parent must not starve a frame's
            # handshake just because this tick took the other branch
            messaged = self._pump_frame_bridge()
            if changed or self._deferred_resources:
                first_resources = self._deferred_resources
                started = time.perf_counter()
                self._deferred_resources = False
                self.load_images(keep_cache=True)
                self.apply_title()
                self._load_frames()
                self.relayout()
                if first_resources:
                    self._load_timings["deferred_resources"] = \
                        (time.perf_counter() - started) * 1000.0
                    self.set_status("완료 (native window)")
            else:
                # sample CSS animations/transitions (a relayout above
                # already sampled inside relayout())
                result = self.animator.on_frame(self.nodes)
                # child frames run their own event loops + animations
                frames_changed = (self.frames.tick(dt)
                                  if self.frames is not None else False)
                if result.damage == "layout":
                    self.relayout()
                elif result.damage == "paint" or frames_changed \
                        or scrolled or messaged:
                    self.repaint()
            if self.animator.active or (
                    self.frames is not None
                    and self.frames.animations_active()):
                # animations want frame pace, not the 80ms page tick;
                # idle pages keep the slower cadence (no timer runaway)
                self._live_next = now + 0.016
        except Exception as exc:
            print(f"[live] tick error: {exc}")

    def _load_frames(self):
        if self.frames is None or self.nodes is None:
            return False
        try:
            changed = self.frames.sync(
                self.nodes, self.url, cancel_token=self._loading_token)
            if self._frames_pending:
                pending, self._frames_pending = self._frames_pending, ()
                self._frames_restoring = True
                try:
                    changed = navigation.restore_frame_state(
                        self.frames, pending) or changed
                finally:
                    self._frames_restoring = False
            return changed
        except net.RequestCancelled:
            raise
        except Exception as exc:
            print(f"[frames] load error: {exc}")
            return False

    def _dispatch_frame_event(self, ridx, event_type):
        try:
            logs, _handled, _prevented = self.renderer.dispatch_event(
                ridx, event_type, False, False, None)
            for line in logs:
                print(f"[js console] {line}")
        except Exception:
            pass

    def apply_title(self):
        title = "GG Browser"
        for node in tree_to_list(self.nodes, []):
            if isinstance(node, Element) and node.tag == "title":
                text = " ".join(
                    c.text for c in node.children if isinstance(c, Text))
                if text.strip():
                    title = text.strip() + " - GG Browser"
                break
        self.win.set_title(title)

    def load_images(self, keep_cache):
        if not keep_cache:
            self.engine.clear_images()
            self._img_by_src = {}
        img_nodes = [n for n in tree_to_list(self.nodes, [])
                     if isinstance(n, Element) and n.tag == "img"
                     and n.attributes.get("src")]
        srcs = [s for s in {n.attributes["src"] for n in img_nodes}
                if s not in self._img_by_src]
        if srcs:
            raw = _fetch_many(
                srcs, self.url, binary=True,
                timeout=self.navigation_timeout,
                cancel_token=self._loading_token,
                network_backend=self.network,
                network_context=self.renderer.network_context)
            for src, data in raw.items():
                try:
                    self._img_by_src[src] = (
                        self.engine.load_image(data) if data else None)
                except Exception:
                    self._img_by_src[src] = None
        for node in img_nodes:
            node._img = self._img_by_src.get(node.attributes["src"])
        textengine.load_svgs(self.nodes)
        textengine.load_background_images(
            self.nodes,
            lambda urls: _fetch_many(
                urls, self.url, binary=True,
                timeout=self.navigation_timeout,
                cancel_token=self._loading_token,
                network_backend=self.network,
                network_context=self.renderer.network_context))

    def logical_size(self):
        """Window size in CSS px. Layout, hit-testing, and the UI all
        work in logical px; only the raster buffer is physical."""
        w, h = self.win.size()
        scale = self.win.scale_factor()
        return w / scale, h / scale

    def repaint(self):
        """Rebuild the display list without relayout — enough for
        paint-only animation frames (opacity, colors, transform)."""
        if self.document is None:
            return
        self.display_list = paint_tree(self.document, [])
        self._list_dirty = True
        self.dirty = True

    def relayout(self):
        if self.nodes is None:
            return
        # re-point iframe boxes after tree rebuilds, then sample
        # animations so layout sees the animated values
        if self.frames is not None:
            self.frames.attach(self.nodes)
        self.animator.on_frame(self.nodes)
        w, h = self.logical_size()
        # width-dependent @media rules must re-evaluate when the window is
        # resized across a breakpoint (styling is otherwise width-agnostic)
        if self._doc is not None and w != getattr(self, "_styled_width", w):
            self._styled_width = w
            self.nodes = self.renderer.frame(w)
            self._remap_focus()
            self.load_images(keep_cache=True)
        viewport_h = h - TOOLBAR_H - STATUS_H
        self.document = DocumentLayout(self.nodes)
        self.document.layout(max(w, 200),
                             viewport_h if viewport_h > 50 else None)
        self.layout_list = layout_tree_to_list(self.document, [])
        self.display_list = paint_tree(self.document, [])
        self._list_dirty = True
        # widest painted extent: absolute boxes (naver's 1190px design)
        # can stick out past the window even though layout used w
        self.content_width = max(
            (o.x + o.width for o in self.layout_list
             if getattr(o, "width", None) is not None), default=w)
        # tallest painted extent. A `height:100vh` root (wikipedia's page
        # container) pins document.height to the viewport even when the
        # article overflows ~10000px below it, so scrolling by the root's
        # own height stops at the first screen. Measure the deepest laid-out
        # box instead so scroll reaches the real bottom of the page.
        self.content_height = max(
            (o.y + o.height for o in self.layout_list
             if getattr(o, "height", None) is not None),
            default=self.document.height + 2 * VSTEP)
        self._push_scroll_state()
        self.clamp_scroll()
        self.dirty = True

    def _push_scroll_state(self):
        """Feed real scroll offsets and content sizes to the JS engine
        so `el.scrollTop`/`scrollHeight` observe the actual scrollers."""
        if self._doc is None:
            return
        try:
            self.renderer.set_scroll_state(
                collect_scroll_state(self.layout_list, self._page_scroller()))
        except Exception:
            pass  # older wheel without the scroll seam

    def _page_scroller(self):
        """The page's own scroller as (top, left, height, width) —
        what window.scrollY and a relative window.scrollBy read."""
        w, h = self.logical_size()
        viewport = max(h - TOOLBAR_H - STATUS_H, 1)
        return (float(self.scroll), float(self.hscroll),
                float(max(self.scroll_extent(), viewport)),
                float(max(self.content_width + HSTEP, w)))

    def _pump_frame_bridge(self):
        """Route this turn's postMessage traffic between the page and
        its frames. Returns True when anything was delivered."""
        if self._doc is None or self.frames is None:
            return False
        try:
            return self.frames.pump_bridge()
        except Exception:
            return False

    def _apply_scroll_writes(self):
        """Apply the scrolling page scripts asked for this turn —
        el.scrollTop/scrollTo/scrollBy, window.scrollTo/scrollBy, and
        el.scrollIntoView()."""
        if self._doc is None:
            return False
        try:
            writes = self.renderer.take_scroll_writes()
            into_view = self.renderer.take_scroll_into_view()
        except Exception:
            return False
        if not writes and not into_view:
            return False
        _w, h = self.logical_size()
        moved, page_target = apply_scroll_requests(
            self.layout_list, writes, into_view,
            max(h - TOOLBAR_H - STATUS_H, 1), self.scroll, self.hscroll)
        if page_target is not None:
            self.scroll = page_target[0]
            self.hscroll = page_target[1]
            self.clamp_scroll()
            moved = True
        if moved:
            self._scroll_state = capture_scroll_state(self.nodes)
            self._push_scroll_state()
        return moved

    def refresh_after_js(self):
        # a click handler may have scheduled fetch/timers — settle them
        self.renderer.settle(timeout=self.navigation_timeout, refresh=False)
        self.nodes = self.renderer.frame(self._styled_width)
        self._remap_focus()
        self._load_frames()
        # a click handler that posted to a frame must reach it inside
        # the same interaction, not one animation frame later
        self._pump_frame_bridge()
        self.load_images(keep_cache=True)
        self.apply_title()
        self.relayout()

    def reload(self):
        if self.url:
            self.load(self.url, add_to_history=False, no_cache=True)

    def go_back(self):
        if self.history_index > 0:
            self.cancel_navigation()
            self._snapshot_history_entry()
            leaving = self.history[self.history_index]
            self.history_index -= 1
            self._restore_history_entry(
                self.history[self.history_index], leaving)

    def go_forward(self):
        if self.history_index < len(self.history) - 1:
            self.cancel_navigation()
            self._snapshot_history_entry()
            leaving = self.history[self.history_index]
            self.history_index += 1
            self._restore_history_entry(
                self.history[self.history_index], leaving)

    def go_home(self):
        self.load_url_string(HOME_URL)

    # ---------- events ----------

    def handle(self, kind, a, b, text):
        if kind == "close":
            self.running = False
        elif kind in ("ready", "resize"):
            self.relayout()
        elif kind == "wheel":
            mx, my = self._mouse_pos
            obj = (self.hit_test(mx + self.hscroll,
                                 my - TOOLBAR_H + self.scroll)
                   if my >= TOOLBAR_H and self.document else None)
            frame = frames.frame_of(obj) if obj else None
            scroller = (find_scrollable(obj, -b, -a)
                        if obj is not None else None)
            if frame is not None and frame.root is not None \
                    and frame.scroll_by(-b, obj.width, obj.height):
                self.repaint()
            elif scroller is not None \
                    and scroll_container_by(scroller, -b, -a):
                # an inner scroll container consumes the wheel until it
                # bottoms out, then the page scrolls (scroll chaining)
                self._scroll_state = capture_scroll_state(self.nodes)
                self.repaint()
            else:
                self.scroll -= b
                self.hscroll -= a
                self.clamp_scroll()
                self.dirty = True
        elif kind == "mouse_move":
            self._mouse_pos = (a, b)
            self.on_motion(a, b)
        elif kind == "mouse_down" and text == "left":
            self.on_click(a, b)
        elif kind == "text":
            if self.url_focused:
                self.url_text = (self.url_text[:self.caret] + text
                                 + self.url_text[self.caret:])
                self.caret += len(text)
                self.dirty = True
            else:
                self.on_text(text)
        elif kind == "key":
            self.on_key(text)

    @staticmethod
    def _ridx_of(node):
        while node is not None:
            ridx = getattr(node, "_ridx", None)
            if ridx is not None:
                return ridx
            node = node.parent
        return None

    def _node_by_ridx(self, ridx):
        if ridx is None or self.nodes is None:
            return None
        return next((node for node in tree_to_list(self.nodes, [])
                     if getattr(node, "_ridx", None) == ridx), None)

    def _remap_focus(self):
        # inner scroll offsets live on nodes the native path rebuilds
        # each tick, so they are restored here alongside the focus mark
        restore_scroll_state(self.nodes, self._scroll_state)
        self.focus_node = self._node_by_ridx(self._focus_ridx)
        if self.focus_node is not None:
            self.focus_node.is_focused = True

    def set_focus(self, node):
        ridx = self._ridx_of(node)
        if ridx == self._focus_ridx and self.focus_node is node:
            return
        if self.focus_node is not None:
            self.focus_node.is_focused = False
        self._focus_ridx = ridx
        self.focus_node = node
        if node is not None:
            node.is_focused = True
        if self._doc is not None:
            self.renderer.set_focus(ridx)
            self.nodes = self.renderer.frame(self._styled_width)
            self._remap_focus()
            self.load_images(keep_cache=True)
        self.relayout()

    @staticmethod
    def _is_descendant(node, ancestor):
        while node is not None:
            if node is ancestor:
                return True
            node = node.parent
        return False

    def _scroll_focus_into_view(self, node):
        boxes = [obj for obj in self.layout_list
                 if self._is_descendant(getattr(obj, "node", None), node)]
        if not boxes:
            return
        top = min(obj.y for obj in boxes)
        bottom = max(obj.y + obj.height for obj in boxes)
        _w, height = self.logical_size()
        viewport = max(height - TOOLBAR_H - STATUS_H, 1)
        if top < self.scroll:
            self.scroll = top
        elif bottom > self.scroll + viewport:
            self.scroll = bottom - viewport
        self.clamp_scroll()
        self.dirty = True

    def focus_next(self, reverse=False):
        node = keyboard.next_focus(
            self.nodes, self.focus_node, reverse=reverse)
        self.set_focus(node)
        node = self.focus_node
        if node is not None:
            self._scroll_focus_into_view(node)
        return node

    def _edit_focused(self, text=None, backspace=False):
        node = self.focus_node
        if not keyboard.is_text_editable(node):
            return False
        value = node.attributes.get("value", "")
        value = value[:-1] if backspace else value + (text or "")
        node.attributes["value"] = value
        if self._doc is not None:
            self.renderer.set_attr(node._ridx, "value", value)
        self.relayout()
        return True

    def on_text(self, text):
        if text == " ":
            action = keyboard.key_action(self.focus_node, "Space")
            if action == "activate":
                self.activate_node(self.focus_node)
                return
            if action == "select-next":
                self.activate_form_control(self.focus_node, select_step=1)
                return
            if action == "text":
                self._edit_focused(text=" ")
                return
            self.scroll += 600
            self.clamp_scroll()
            self.dirty = True
            return
        self._edit_focused(text=text)

    def on_key(self, name):
        if self.url_focused:
            if name in ("Tab", "ShiftTab"):
                self.url_focused = False
                self.focus_next(reverse=name == "ShiftTab")
            elif name == "Enter":
                self.url_focused = False
                self.load_url_string(self.url_text)
            elif name == "Backspace" and self.caret > 0:
                self.url_text = (self.url_text[:self.caret - 1]
                                 + self.url_text[self.caret:])
                self.caret -= 1
            elif name == "Delete":
                self.url_text = (self.url_text[:self.caret]
                                 + self.url_text[self.caret + 1:])
            elif name == "ArrowLeft":
                self.caret = max(0, self.caret - 1)
            elif name == "ArrowRight":
                self.caret = min(len(self.url_text), self.caret + 1)
            elif name == "Home":
                self.caret = 0
            elif name == "End":
                self.caret = len(self.url_text)
            elif name == "Escape":
                self.url_focused = False
                self.url_text = str(self.url) if self.url else ""
            self.dirty = True
            return
        if name in ("Tab", "ShiftTab"):
            self.focus_next(reverse=name == "ShiftTab")
            return
        if name == "Escape" and self.focus_node is not None:
            self.set_focus(None)
            return
        if name == "Enter" and self.focus_node is not None:
            action = keyboard.key_action(self.focus_node, "Enter")
            if action == "activate":
                self.activate_node(self.focus_node)
            elif action == "submit":
                self.submit_form(self.focus_node)
            elif action == "newline":
                self._edit_focused(text="\n")
            elif action == "select-next":
                self.activate_form_control(self.focus_node, select_step=1)
            return
        if name == "Backspace" and self._edit_focused(backspace=True):
            return
        # arrows drive a focused <select> before they scroll the page
        if self.focus_node is not None and name.startswith("Arrow"):
            action = keyboard.key_action(self.focus_node, name)
            if action in ("select-next", "select-prev"):
                self.activate_form_control(
                    self.focus_node,
                    select_step=1 if action == "select-next" else -1)
                return
        if name == "ArrowDown":
            self.scroll += SCROLL_STEP
        elif name == "ArrowUp":
            self.scroll -= SCROLL_STEP
        elif name == "ArrowRight":
            self.hscroll += SCROLL_STEP
        elif name == "ArrowLeft":
            self.hscroll -= SCROLL_STEP
        elif name == "PageDown":
            self.scroll += 600
        elif name == "PageUp":
            self.scroll -= 600
        elif name == "Home":
            self.scroll = 0
            self.hscroll = 0
        else:
            return
        self.clamp_scroll()
        self.dirty = True

    def hit_test(self, x, y):
        return hit_test_at(self.layout_list, x, y, self.scroll)

    def find_link(self, node):
        while node:
            if (isinstance(node, Element) and node.tag == "a"
                    and "href" in node.attributes):
                return node.attributes["href"]
            node = node.parent
        return None

    def on_click(self, x, y):
        if y < TOOLBAR_H:
            for x1, x2, _label, action in self._buttons:
                if x1 <= x < x2:
                    action()
                    return
            bx1, _by1, bx2, _by2 = self._urlbar
            if bx1 <= x < bx2:
                self.set_focus(None)
                self.url_focused = True
                self.caret = len(self.url_text)
                self.dirty = True
            return
        if self.url_focused:
            self.url_focused = False
            self.dirty = True
        obj = self.hit_test(x + self.hscroll, y - TOOLBAR_H + self.scroll)
        if not obj:
            self.set_focus(None)
            return
        # clicks over an <iframe> box route into the child document
        frame = frames.frame_of(obj)
        if frame is not None and frame.root is not None:
            cx, cy = frames.child_coords(
                obj, x + self.hscroll, y - TOOLBAR_H + self.scroll, frame)
            if frame.click_at(cx, cy, obj.width, obj.height):
                self.repaint()
            return
        clicked_ridx = self._ridx_of(obj.node)
        self.set_focus(keyboard.focus_target(obj.node))
        default_node = self._node_by_ridx(clicked_ridx) or obj.node
        return self.activate_node(default_node)

    def activate_node(self, default_node):
        """Dispatch click and then run the element's browser default action."""
        if self._doc is not None:
            target = default_node
            while target is not None and not (
                    isinstance(target, Element)
                    and hasattr(target, "_ridx")):
                target = target.parent
            if target is not None:
                result = self.renderer.dispatch_click(target._ridx)
                if len(result) == 3:
                    logs, handled, prevented = result
                else:  # pre-preventDefault ggcore wheel
                    logs, handled = result
                    prevented = handled
                for line in logs:
                    print(f"[js console] {line}")
                if handled:
                    self.refresh_after_js()
                    default_node = next((node for node in
                                         tree_to_list(self.nodes, [])
                                         if getattr(node, "_ridx", None)
                                         == target._ridx), default_node)
                if prevented:
                    # preventDefault() (or onclick returning false)
                    # suppresses the default navigation
                    return
        href = self.find_link(default_node)
        if href and not href.startswith(("javascript:", "mailto:")):
            try:
                self.load(self.url.resolve(href))
            except Exception as e:
                self.set_status(f"이동 실패: {e}")
            return
        input_node = forms.find_input(default_node)
        if input_node is not None \
                and input_node.attributes.get("type", "").lower() == "file":
            self.set_status("파일 선택은 tkinter 셸에서 지원합니다")
            return
        checkable = forms.find_checkable(default_node)
        select = forms.find_select(default_node)
        resetter = forms.find_resetter(default_node)
        submitter = forms.find_submitter(default_node)
        if checkable is not None or select is not None \
                or resetter is not None or submitter is not None:
            self.activate_form_control(
                checkable or select or resetter or submitter)

    def submit_form(self, node, submitter=None):
        form = forms.find_form(node)
        if form is None:
            return
        try:
            if submitter is None:
                submitter = next((candidate for candidate in
                                  tree_to_list(self.nodes, [])
                                  if forms.find_submitter(candidate)
                                  is candidate
                                  and forms.form_owner(candidate) is form),
                                 None)
            activation = forms.activate_submission(
                form, self.url, submitter=submitter,
                dispatch_event=self._dispatch_form_event,
                refresh_tree=self._fresh_form_tree)
            self._finish_form_activation(activation)
        except Exception as exc:
            self.set_status(f"폼 제출 실패: {exc}")

    def activate_form_control(self, control, select_step=1):
        try:
            activation = forms.activate_control(
                control, self.url, self._form_defaults,
                dispatch_event=self._dispatch_form_event,
                refresh_tree=self._fresh_form_tree,
                set_attr=self.renderer.set_attr,
                remove_attr=self.renderer.remove_attr,
                select_step=select_step)
            self._finish_form_activation(activation)
        except Exception as exc:
            self.set_status(f"폼 동작 실패: {exc}")

    def _dispatch_form_event(self, *args):
        result = self.renderer.dispatch_event(*args)
        for line in result[0]:
            print(f"[js console] {line}")
        return result

    def _fresh_form_tree(self):
        return native.build_tree(self.renderer.export())

    def _finish_form_activation(self, activation):
        if activation is None:
            return
        if activation.invalid:
            token = activation.invalid[0]
            first = token if isinstance(token, Element) \
                else self._node_by_ridx(token)
            message = forms.validation_message(first) if first else ""
            if first is not None:
                self.set_focus(first)
            self.set_status(message or "폼 값을 확인하세요")
            if activation.handled:
                self.refresh_after_js()
            return
        if activation.prevented:
            if activation.handled:
                self.refresh_after_js()
            return
        if activation.changed:
            self.nodes = self.renderer.frame(self._styled_width)
            self._remap_focus()
            self.load_images(keep_cache=True)
            self.relayout()
            return
        submission = activation.submission
        if submission is not None:
            self.load(
                self.url.resolve(submission.target),
                method=submission.method, body=submission.body,
                headers=submission.headers)

    def on_motion(self, x, y):
        now = time.monotonic()
        if now - self._last_motion < 0.03:
            return
        self._last_motion = now
        href = None
        base = self.url
        if y >= TOOLBAR_H:
            obj = self.hit_test(x + self.hscroll,
                                y - TOOLBAR_H + self.scroll)
            href = self.find_link(obj.node) if obj else None
            frame = frames.frame_of(obj) if obj else None
            if frame is not None and frame.root is not None:
                hit = frames.hit_frame(obj, x + self.hscroll,
                                       y - TOOLBAR_H + self.scroll)
                href = frame.find_link(hit[1].node) if hit else None
                base = frame.url
        self.win.set_cursor_pointer(href is not None)
        status = str(base.resolve(href)) if href and base else ""
        if status != self.status:
            self.status = status
            self.dirty = True

    def set_status(self, text):
        if text != self.status:
            self.status = text
            self.dirty = True

    # ---------- frame ----------

    def scroll_extent(self):
        """Total scrollable page height in CSS px. Uses the deepest laid-out
        box, not document.height, so content overflowing a viewport-pinned
        (height:100vh) root stays reachable."""
        doc = (self.document.height + 2 * VSTEP) if self.document else 0
        return max(doc, self.content_height + VSTEP)

    def clamp_scroll(self):
        w, h = self.logical_size()
        content_h = max(h - TOOLBAR_H - STATUS_H, 1)
        if not self.document:
            self.scroll = 0
            self.hscroll = 0
            return
        max_scroll = max(self.scroll_extent() - content_h, 0)
        self.scroll = min(max(0, self.scroll), max_scroll)
        max_hscroll = max(self.content_width + HSTEP - w, 0)
        self.hscroll = min(max(0, self.hscroll), max_hscroll)

    def chrome_cmds(self, w, h):
        """Toolbar, status bar, and scrollbars — viewport-coordinate
        overlay drawn on top of the scrolled page content."""
        content_h = h - TOOLBAR_H - STATUS_H
        cmds = []

        # scrollbar
        if self.document:
            content = self.scroll_extent()
            if content > content_h:
                bar_h = max(content_h * content_h / content, 24)
                max_scroll = content - content_h
                frac = self.scroll / max_scroll if max_scroll else 0
                y = TOOLBAR_H + frac * (content_h - bar_h)
                cmds.append((0, w - 9, y, w - 3, y + bar_h,
                             (192, 192, 192), 0.0, 0, ""))
            # horizontal scrollbar (content wider than the window)
            content_w = self.content_width + HSTEP
            if content_w > w:
                bar_w = max(w * w / content_w, 24)
                max_h = content_w - w
                frac = self.hscroll / max_h if max_h else 0
                x0 = frac * (w - bar_w)
                y0 = h - STATUS_H - 9
                cmds.append((0, x0, y0, x0 + bar_w, y0 + 6,
                             (192, 192, 192), 0.0, 0, ""))

        # toolbar
        cmds.append((0, 0, 0, w, TOOLBAR_H, TOOLBAR_BG, 0.0, 0, ""))
        cmds.append((2, 0, TOOLBAR_H - 0.5, w, TOOLBAR_H - 0.5,
                     BORDER, 1.0, 0, ""))
        self._buttons = []
        x = 10
        back_ok = self.history_index > 0
        fwd_ok = self.history_index < len(self.history) - 1
        for label, action, enabled in (
                ("←", self.go_back, back_ok),
                ("→", self.go_forward, fwd_ok),
                ("⟳", self.reload, True),
                ("✕", self.cancel_navigation, self._navigation.active),
                ("⌂", self.go_home, True)):
            color = INK if enabled else (170, 170, 170)
            cmds.append((1, x + 6, 10, 0.0, 0.0, color, 19.0,
                         self.ui_font.id, label))
            self._buttons.append((x, x + 32, label, action))
            x += 36

        # URL bar
        bar_x1, bar_y1 = x + 6, 8
        bar_x2, bar_y2 = w - 12, TOOLBAR_H - 8
        self._urlbar = (bar_x1, bar_y1, bar_x2, bar_y2)
        cmds.append((0, bar_x1, bar_y1, bar_x2, bar_y2,
                     (255, 255, 255), 0.0, 0, ""))
        if self.url_focused:
            cmds.append((2, bar_x1, bar_y2, bar_x2, bar_y2,
                         (43, 90, 166), 2.0, 0, ""))
        text_x = bar_x1 + 8
        text_y = bar_y1 + 4
        cmds.append((1, text_x, text_y, 0.0, 0.0, INK, 15.0,
                     self.ui_font.id, self.url_text))
        if self.url_focused:
            caret_x = text_x + measure(self.ui_font,
                                       self.url_text[:self.caret])
            cmds.append((2, caret_x, text_y, caret_x, text_y + 18,
                         INK, 1.0, 0, ""))

        # status bar
        cmds.append((0, 0, h - STATUS_H, w, h, STATUS_BG, 0.0, 0, ""))
        cmds.append((2, 0, h - STATUS_H, w, h - STATUS_H,
                     BORDER, 1.0, 0, ""))
        if self.status:
            cmds.append((1, 8, h - STATUS_H + 4, 0.0, 0.0,
                         (80, 80, 80), 12.0, self.ui_small.id,
                         self.status))
        return cmds

    def render_frame(self):
        pw, ph = self.win.size()  # physical px: buffer + surface size
        w, h = self.logical_size()
        if w < 50 or h < TOOLBAR_H + STATUS_H + 10:
            return
        scale = self.win.scale_factor()
        # the page list lives in Rust (device px, document coords);
        # re-push only when paint or DPI changed — scroll frames just
        # pass offsets (M6: no per-frame serialization)
        if self._list_dirty or scale != self._pushed_scale:
            self.engine.set_display_list(scale_cmds(
                [cmd.native(0, 0.0) for cmd in self.display_list],
                scale))
            self._pushed_scale = scale
            self._list_dirty = False
        overlay = scale_cmds(self.chrome_cmds(w, h), scale)
        buf = self.engine.render_frame_raw(
            pw, ph, (255, 255, 255),
            self.hscroll * scale, (self.scroll - TOOLBAR_H) * scale,
            overlay)
        self.win.present(pw, ph, buf)
        self.dirty = False


def run(url_string=None):
    import ggcore
    if not textengine.available():
        raise SystemExit("native shell requires the ggcore module")

    win = ggcore.NativeWindow(1100, 780, "GG Browser")
    shell = Shell(win)
    pending = url_string or HOME_URL
    loaded = False

    try:
        while shell.running:
            for kind, a, b, text in win.pump(16):
                shell.handle(kind, a, b, text)
                if kind == "ready" and not loaded:
                    loaded = True
                    shell.load_url_string(pending)
            shell.poll_navigation()
            if shell.dirty:
                shell.render_frame()
            # Run page work only after a dirty frame has been presented, so
            # a slow fetch/resource pass cannot suppress the first paint.
            shell.tick_live()
    finally:
        shell._navigation.shutdown()
        if shell.frames is not None:
            shell.frames.dispose()
        shutdown = getattr(shell.renderer, "shutdown", None)
        if shutdown is not None:
            shutdown()
        else:
            shell.renderer.close()
