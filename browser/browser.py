"""Browser chrome: window, toolbar, scrolling, navigation, hit-testing."""

import os
import time
import tkinter
import tkinter.font
import traceback
from concurrent.futures import ThreadPoolExecutor

from . import (animation, forms, frames, keyboard, native, navigation,
               net, textengine, webfonts)
from .html_parser import Element, HTMLParser, Text, tree_to_list
from .css_parser import CSSParser
from .draw import DrawStickyPop, DrawStickyPush
from .style import RuleIndex, cascade_priority, default_rules, style
from .layout import (VSTEP, BlockLayout, DocumentLayout, ImageLayout,
                     TextLayout, find_scrollable, hit_test_at,
                     layout_tree_to_list, paint_tree,
                     scroll_container_by)
from .pages import error_page
from .network_backend import default_network_backend
from .renderer_session import create_renderer_session

SCROLL_STEP = 90
HOME_URL = "about:home"


class Browser:
    def __init__(self, network_backend=None, process_model=None):
        self.network = network_backend or default_network_backend()
        model = process_model or os.environ.get("GG_PROCESS_MODEL", "local")
        self.renderer = (create_renderer_session(
            self.network, process_model=model, timeout=net.DEFAULT_TIMEOUT)
            if native.available() else None)
        self.window = tkinter.Tk()
        self.window.title("GG Browser")
        self.window.geometry("1100x780")
        try:
            self.window.iconbitmap(default="")
        except tkinter.TclError:
            pass

        self._build_toolbar()

        self.canvas = tkinter.Canvas(self.window, bg="white",
                                     highlightthickness=0)
        self.canvas.pack(fill="both", expand=True)

        self.status = tkinter.Label(self.window, text="", anchor="w",
                                    bg="#f0f0f0", fg="#333333", padx=8)
        self.status.pack(fill="x", side="bottom")

        self.scroll = 0
        self.document = None
        self.layout_list = []
        self.display_list = []
        self.focus_node = None
        self.history = []
        self.history_index = -1
        self.navigation_timeout = net.DEFAULT_TIMEOUT
        self._navigation = navigation.NavigationController(
            network_backend=self.network)
        self._navigation_poll = None
        self._loading_token = None
        self.url = None
        self._doc = None            # native (Rust) document handle
        self._css_sources = []
        self._img_by_src = {}
        self._load_timings = {}
        self._deferred_resources = False
        self.default_rules = default_rules()
        self._layout_width = 0
        self._resize_job = None
        self.animator = animation.AnimationEngine()
        self._anim_job = None
        self.frames = None

        self.canvas.bind("<Button-1>", self.on_click)
        self.canvas.bind("<Key>", self.on_key)
        self.canvas.bind("<Motion>", self.on_motion)
        self.canvas.bind("<MouseWheel>", self.on_mousewheel)
        self.canvas.bind("<Configure>", self.on_configure)
        self.window.bind("<Down>", lambda e: self.scroll_by(SCROLL_STEP))
        self.window.bind("<Up>", lambda e: self.scroll_by(-SCROLL_STEP))
        self.window.bind("<Next>", lambda e: self.scroll_by(600))
        self.window.bind("<Prior>", lambda e: self.scroll_by(-600))
        self.window.bind("<Home>", lambda e: self.scroll_to(0))
        self.window.bind("<Control-l>",
                         lambda e: (self.url_entry.focus_set(),
                                    self.url_entry.select_range(0, "end")))
        self.window.bind("<Control-r>", lambda e: self.reload())
        self.window.bind("<Alt-Left>", lambda e: self.go_back())
        self.window.bind("<Alt-Right>", lambda e: self.go_forward())

    def _build_toolbar(self):
        bar = tkinter.Frame(self.window, bg="#e8e8e8", pady=4, padx=6)
        bar.pack(fill="x", side="top")

        btn_font = tkinter.font.Font(family="Segoe UI", size=11)

        def make_btn(text, cmd):
            b = tkinter.Button(bar, text=text, command=cmd, font=btn_font,
                               relief="flat", bg="#e8e8e8",
                               activebackground="#d0d0d0", padx=8)
            b.pack(side="left")
            return b

        self.back_btn = make_btn("←", self.go_back)
        self.fwd_btn = make_btn("→", self.go_forward)
        make_btn("⟳", self.reload)
        make_btn("✕", self.cancel_navigation)
        make_btn("⌂", lambda: self.load_url_string(HOME_URL))

        self.url_var = tkinter.StringVar()
        self.url_entry = tkinter.Entry(
            bar, textvariable=self.url_var,
            font=tkinter.font.Font(family="Segoe UI", size=11),
            relief="flat", bg="white")
        self.url_entry.pack(side="left", fill="x", expand=True,
                            padx=8, ipady=5)
        self.url_entry.bind("<Return>", self.on_url_enter)

    # ---------- navigation ----------

    def on_url_enter(self, event):
        text = self.url_var.get().strip()
        if not text:
            return
        if "://" not in text and not text.startswith("about:"):
            text = "https://" + text
        self.load_url_string(text)
        self.canvas.focus_set()

    def load_url_string(self, text):
        try:
            url = net.URL(text)
        except Exception:
            url = net.URL("about:home")
        self.load(url)

    def load(self, url, add_to_history=True, no_cache=False, *,
             method="GET", body=None, headers=None, timeout=None):
        self._snapshot_history_entry()
        self.set_status(f"로딩 중... {url}")
        self.window.update_idletasks()
        initiator = getattr(self, "url", None)
        if initiator is not None and not getattr(initiator, "host", None):
            initiator = None
        timeout = self.navigation_timeout if timeout is None else timeout
        context = {
            "url": url, "add_to_history": add_to_history,
            "timeout": timeout,
        }

        def fetch(token):
            return self.network.request_text(
                url, no_cache=no_cache, method=method, body=body,
                headers=headers, site_for_cookies=initiator,
                top_level_navigation=True, timeout=timeout,
                cancel_token=token)

        pending = self._navigation.start(fetch, context)
        self._schedule_navigation_poll()
        return pending.future

    def _schedule_navigation_poll(self):
        if self._navigation_poll is None:
            self._navigation_poll = self.window.after(
                16, self._poll_navigation)

    def _poll_navigation(self):
        self._navigation_poll = None
        pending = self._navigation.take_ready()
        if pending is None:
            if self._navigation.active:
                self._schedule_navigation_poll()
            return
        context = pending.context
        url = context["url"]
        try:
            _headers, page_body, url = pending.future.result()
            # url is now the post-redirect URL: relative links, history
            # and the address bar must all use it as the base
        except net.RequestCancelled:
            self.set_status("탐색 취소됨")
            return
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
        self._update_nav_buttons()

    def cancel_navigation(self):
        if self._navigation.cancel():
            self.set_status("탐색 취소됨")

    def _snapshot_history_entry(self):
        if not (0 <= self.history_index < len(self.history)):
            return
        entry = self.history[self.history_index]
        entry.scroll = float(self.scroll)
        entry.form_state = navigation.capture_form_state(
            getattr(self, "nodes", None))

    def _restore_history_entry(self, entry):
        self.render_page(entry.url, entry.body)
        changed = navigation.restore_form_state(
            self.nodes, entry.form_state,
            set_attr=(self.renderer.set_attr if self._doc is not None else None),
            remove_attr=(self.renderer.remove_attr
                         if self._doc is not None else None))
        if changed and self._doc is not None:
            self.nodes = self.renderer.frame()
        if changed:
            self.relayout()
        self.scroll = entry.scroll
        self.clamp_scroll()
        self.draw()
        self._update_nav_buttons()

    def render_page(self, url, body):
        render_started = time.perf_counter()
        self._load_timings = {}
        self.url = url
        self.url_var.set(str(url))
        self._reset_interaction()
        if self.frames is not None:
            self.frames.dispose()
        self.frames = frames.FrameManager(
            self.network, top_url=url, timeout=self.navigation_timeout)

        if native.available():
            # Rust fast path: parse + gg-js + cascade + style in ggcore.
            load_timings = {}
            self.renderer.timeout = self.navigation_timeout
            self.nodes, self._doc, self._css_sources, js_logs = \
                self.renderer.commit(
                    url, body, timings=load_timings,
                    cancel_token=self._loading_token)
            self._load_timings = load_timings
            self._hover_rules = any(
                ":hover" in s for s in self._css_sources)
            self._focus_rules = any(
                ":focus" in s for s in self._css_sources)
            for line in js_logs:
                print(f"[js console] {line}")
            if js_logs:
                self.set_status(f"JS 콘솔 {len(js_logs)}줄 (터미널 참고)")
        else:
            # Pure-Python fallback
            self._doc = None
            self.nodes = HTMLParser(body).parse()
            self._drop_focus()
            page_rules, self._css_sources = \
                self.collect_styles(self.nodes, url)
            rules = self.default_rules + page_rules
            rules = sorted(rules, key=cascade_priority)
            # Style depends only on the DOM and CSS, not on window size:
            # compute it once per page load, never on resize.
            self._py_rule_index = RuleIndex(rules)
            style(self.nodes, self._py_rule_index)
            self._hover_rules = any(
                ":hover" in repr(sel) for sel, _ in rules)
            self._focus_rules = any(
                ":focus" in repr(sel) for sel, _ in rules)

        self._form_defaults = forms.capture_defaults(self.nodes)

        title = "GG Browser"
        for node in tree_to_list(self.nodes, []):
            if isinstance(node, Element) and node.tag == "title":
                text = " ".join(
                    c.text for c in node.children if isinstance(c, Text))
                if text.strip():
                    title = text.strip() + " - GG Browser"
                break
        self.window.title(title)

        if self._doc is not None:
            self.frames.dispatch_event = self._dispatch_frame_event

        # First paint must not wait for every async request, image, and web
        # font. The live loop materializes those after this frame is visible.
        self._deferred_resources = textengine.available()
        self.animator.reset(self._css_sources)
        self.scroll = 0
        self.relayout()
        self._load_timings["first_paint"] = \
            (time.perf_counter() - render_started) * 1000.0
        if self._load_timings:
            summary = " ".join(
                f"{name}={value:.1f}ms"
                for name, value in self._load_timings.items())
            print(f"[perf] {url} {summary}")
        mode = []
        if native.available():
            mode.append("rust")
        if textengine.available():
            mode.append("raster")
        self.set_status("첫 화면" +
                        (f" ({'+'.join(mode)})" if mode else "") +
                        (" · 나머지 로딩 중" if self._deferred_resources
                         else ""))
        self._start_live_loop()

    # --- M4: live event loop ------------------------------------------
    # After load, keep the page alive: tick the JS event loop at frame
    # pace (timers/rAF fire in real time) and re-style/re-layout only
    # when the DOM version changes. Full recompute per change for now —
    # the engine core is fast enough (~ms for shell pages); partial
    # invalidation comes later.

    _LIVE_INTERVAL_MS = 80
    _LIVE_IDLE_AFTER = 150      # ~12s quiet -> back off, don't stop
    _LIVE_IDLE_INTERVAL_MS = 500  # late timers (carousels) still fire
    _ANIM_INTERVAL_MS = 33      # frame pace while animations run

    def _start_live_loop(self):
        self._live_gen = getattr(self, "_live_gen", 0) + 1
        if (self._doc is None or not hasattr(self._doc, "tick")
                or not native.async_available()):
            gen = self._live_gen
            if self._deferred_resources:
                self.window.after(
                    1, lambda: self._finish_deferred_resources(gen))
            else:
                # frames still load after the first paint
                self.window.after(
                    1, lambda: self._frames_after_first_paint(gen))
            self._ensure_anim_loop()
            return
        self._dom_version = self.renderer.state().dom_version
        self._live_idle = 0
        import time
        self._live_last = time.monotonic()
        gen = self._live_gen
        self._live_job = self.window.after(self._LIVE_INTERVAL_MS,
                                           lambda: self._live_tick(gen))

    def _finish_deferred_resources(self, gen):
        """Load non-critical resources only after the first frame exists."""
        if gen != getattr(self, "_live_gen", 0) \
                or not self._deferred_resources:
            return
        started = time.perf_counter()
        self._deferred_resources = False
        self.load_images(self.nodes, self.url, keep_cache=True)
        self._load_web_fonts(self.url)
        self._load_frames()
        self.relayout()
        self._load_timings["deferred_resources"] = \
            (time.perf_counter() - started) * 1000.0
        self.set_status("완료")

    def _frames_after_first_paint(self, gen):
        if gen != getattr(self, "_live_gen", 0):
            return
        if self._load_frames():
            self.relayout()

    def _load_frames(self):
        """Fetch/commit new or changed <iframe> documents. Returns True
        when any frame content changed."""
        if self.frames is None or not hasattr(self, "nodes"):
            return False
        try:
            return self.frames.sync(
                self.nodes, self.url, cancel_token=self._loading_token)
        except net.RequestCancelled:
            raise
        except Exception as exc:
            print(f"[frames] load error: {exc}")
            return False

    def _dispatch_frame_event(self, ridx, event_type):
        """Fire an iframe lifecycle event (load/error) in the parent doc."""
        try:
            logs, _handled, _prevented = self.renderer.dispatch_event(
                ridx, event_type, False, False, None)
            for line in logs:
                print(f"[js console] {line}")
        except Exception:
            pass

    def _live_tick(self, gen):
        self._live_job = None
        if gen != getattr(self, "_live_gen", 0) or self._doc is None:
            return  # a newer page took over
        import time
        try:
            # advance virtual time by real elapsed time (capped), so
            # the idle backoff doesn't slow the page's clock down
            now = time.monotonic()
            dt = min((now - self._live_last) * 1000.0, 1000.0)
            self._live_last = now
            update = self.renderer.tick(dt)
            for line in update.logs:
                print(f"[js live] {line}")
            changed = update.dom_changed
            if changed:
                self._live_idle = 0
                self.nodes = self.renderer.frame()
                self._remap_marks()
            if changed or self._deferred_resources:
                first_resources = self._deferred_resources
                started = time.perf_counter()
                self._deferred_resources = False
                self.load_images(self.nodes, self.url, keep_cache=True)
                if first_resources:
                    self._load_web_fonts(self.url)
                self._load_frames()
                self.relayout()
                if first_resources:
                    self._load_timings["deferred_resources"] = \
                        (time.perf_counter() - started) * 1000.0
                    self.set_status("완료")
            else:
                self._live_idle += 1
                # sample CSS animations/transitions; a full relayout
                # (above) already sampled inside relayout()
                result = self.animator.on_frame(self.nodes)
                # child frames run their own event loops + animations
                frames_changed = (self.frames.tick(dt)
                                  if self.frames is not None else False)
                if result.damage == "layout":
                    self.relayout()
                elif result.damage == "paint" or frames_changed:
                    self.repaint()
        except Exception as e:
            print(f"[live] tick error: {e}")
            return
        if self.animator.active or (
                self.frames is not None
                and self.frames.animations_active()):
            # animations want frame pace, but never slower than the
            # idle backoff would allow — and idle pages keep backing off
            wait = self._ANIM_INTERVAL_MS
        else:
            wait = (self._LIVE_IDLE_INTERVAL_MS
                    if self._live_idle >= self._LIVE_IDLE_AFTER
                    else self._LIVE_INTERVAL_MS)
        self._live_job = self.window.after(
            wait, lambda: self._live_tick(gen))

    def load_images(self, nodes, url, keep_cache=False):
        """Fetch (parallel) and decode (Rust) every <img> on the page."""
        if not textengine.available():
            return
        engine = textengine.engine()
        if not keep_cache:
            engine.clear_images()
            self._img_by_src = {}
        img_nodes = [n for n in tree_to_list(nodes, [])
                     if isinstance(n, Element) and n.tag == "img"
                     and n.attributes.get("src")]
        srcs = [s for s in {n.attributes["src"] for n in img_nodes}
                if s not in self._img_by_src]

        def fetch(src):
            try:
                _, data = self.network.request_raw(
                    url.resolve(src), site_for_cookies=url,
                    top_level_navigation=False,
                    timeout=self.navigation_timeout,
                    cancel_token=self._loading_token,
                    context=(self.renderer.network_context
                             if self.renderer else None))
                return data
            except net.RequestCancelled:
                raise
            except Exception:
                return b""

        if srcs:
            self.set_status(f"이미지 {len(srcs)}개 로딩...")
            self.window.update_idletasks()
            with ThreadPoolExecutor(max_workers=6) as pool:
                raw = dict(zip(srcs, pool.map(fetch, srcs)))
            for src, data in raw.items():
                try:
                    self._img_by_src[src] = (
                        engine.load_image(data) if data else None)
                except Exception:
                    self._img_by_src[src] = None
        for node in img_nodes:
            node._img = self._img_by_src.get(node.attributes["src"])
        textengine.load_svgs(nodes)

        def fetch_raw(urls):
            with ThreadPoolExecutor(max_workers=6) as pool:
                return dict(zip(urls, pool.map(fetch, urls)))

        textengine.load_background_images(nodes, fetch_raw)

    def fetch_scripts(self, srcs, url):
        """Fetch external <script src> files in parallel: {src: code}."""
        fetched = {}
        if not srcs:
            return fetched
        def fetch(src):
            try:
                _, code = self.network.request(
                    url.resolve(src), site_for_cookies=url,
                    top_level_navigation=False,
                    timeout=self.navigation_timeout,
                    cancel_token=self._loading_token,
                    context=(self.renderer.network_context
                             if self.renderer else None))
                return code
            except net.RequestCancelled:
                raise
            except Exception:
                return ""

        with ThreadPoolExecutor(max_workers=6) as pool:
            for src, code in zip(srcs, pool.map(fetch, srcs)):
                fetched.setdefault(src, code)
        return fetched

    def fetch_stylesheets(self, hrefs, url):
        """Fetch external stylesheets in parallel: {href: css_text}.
        UI-silent on purpose: native.load_document calls this from a
        prefetch thread, and tkinter must stay on the main thread."""
        fetched = {}
        if not hrefs:
            return fetched

        def fetch(href):
            try:
                _, css = self.network.request(
                    url.resolve(href), site_for_cookies=url,
                    top_level_navigation=False,
                    timeout=self.navigation_timeout,
                    cancel_token=self._loading_token,
                    context=(self.renderer.network_context
                             if self.renderer else None))
                return css
            except net.RequestCancelled:
                raise
            except Exception:
                return ""

        with ThreadPoolExecutor(max_workers=6) as pool:
            for href, css in zip(hrefs, pool.map(fetch, hrefs)):
                fetched.setdefault(href, css)
        return fetched

    def collect_styles(self, nodes, url):
        """Gather <style> contents and <link> hrefs in document order.
        Returns (rules, raw_css_texts) — the raw texts feed @keyframes
        parsing (at-rules never reach the rule cascade)."""
        entries = []
        for node in tree_to_list(nodes, []):
            if not isinstance(node, Element):
                continue
            if node.tag == "style":
                css = "".join(
                    c.text for c in node.children if isinstance(c, Text))
                entries.append(("inline", css))
            elif (node.tag == "link"
                  and node.attributes.get("rel", "").casefold() == "stylesheet"
                  and "href" in node.attributes):
                entries.append(("link", node.attributes["href"]))

        hrefs = [val for kind, val in entries if kind == "link"]
        fetched = self.fetch_stylesheets(hrefs, url)

        # Parse in document order so the cascade stays correct
        rules = []
        texts = []
        for kind, val in entries:
            css = val if kind == "inline" else fetched.get(val, "")
            if css:
                texts.append(css)
                rules.extend(CSSParser(css).parse())
        return rules, texts

    def relayout(self):
        if not hasattr(self, "nodes"):
            return
        # re-point iframe boxes at their child documents (tree rebuilds
        # produce fresh node objects), then sample animations so layout
        # sees the animated values
        if self.frames is not None:
            self.frames.attach(self.nodes)
        self.animator.on_frame(self.nodes)
        width = max(self.canvas.winfo_width(), 200)
        height = self.canvas.winfo_height()
        self._layout_width = width
        self.document = DocumentLayout(self.nodes)
        self.document.layout(width, height if height > 50 else None)
        self.layout_list = layout_tree_to_list(self.document, [])
        self.display_list = paint_tree(self.document, [])
        self._push_display_list()
        self._push_layout_rects()
        self.clamp_scroll()
        self.draw()
        self._ensure_anim_loop()

    def _ensure_anim_loop(self):
        """Keep animations moving on the pure-Python path (no gg-js
        live loop). The native path samples from _live_tick instead."""
        if not self.animator.active:
            return
        if (self._doc is not None and hasattr(self._doc, "tick")
                and native.async_available()):
            # _live_tick drives the frames — but if it backed off to
            # the idle interval, pull the next tick forward so a fresh
            # transition doesn't wait half a second for its first frame
            job = getattr(self, "_live_job", None)
            if job is not None:
                self.window.after_cancel(job)
                gen = self._live_gen
                self._live_job = self.window.after(
                    self._ANIM_INTERVAL_MS,
                    lambda: self._live_tick(gen))
            return
        if self._anim_job is not None:
            return
        self._anim_job = self.window.after(
            self._ANIM_INTERVAL_MS, self._anim_tick)

    def _anim_tick(self):
        # _anim_job gates scheduling, so at most one loop exists; a
        # navigation resets the animator, which stops it naturally
        self._anim_job = None
        if not hasattr(self, "nodes"):
            return
        result = self.animator.on_frame(self.nodes)
        if result.damage == "layout":
            self.relayout()  # re-arms the loop itself
            return
        if result.damage == "paint":
            self.repaint()
        self._ensure_anim_loop()

    def reload(self):
        if self.url:
            self.load(self.url, add_to_history=False, no_cache=True)

    def go_back(self):
        if self.history_index > 0:
            self.cancel_navigation()
            self._snapshot_history_entry()
            self.history_index -= 1
            self._restore_history_entry(self.history[self.history_index])

    def go_forward(self):
        if self.history_index < len(self.history) - 1:
            self.cancel_navigation()
            self._snapshot_history_entry()
            self.history_index += 1
            self._restore_history_entry(self.history[self.history_index])

    def _update_nav_buttons(self):
        self.back_btn.config(
            state="normal" if self.history_index > 0 else "disabled")
        self.fwd_btn.config(
            state="normal"
            if self.history_index < len(self.history) - 1 else "disabled")

    # ---------- drawing ----------

    def _load_web_fonts(self, url):
        """Register loadable @font-face fonts before the first
        layout; a hit invalidates the font cache so the new family
        resolves."""
        css_texts = list(self._css_sources)
        if not css_texts:
            for n in tree_to_list(self.nodes, []):
                if isinstance(n, Element) and n.tag == "style":
                    css_texts.append(" ".join(
                        c.text for c in n.children if isinstance(c, Text)))
        def fetch(u):
            _, body = self.network.request_raw(
                url.resolve(u), site_for_cookies=url,
                top_level_navigation=False,
                timeout=self.navigation_timeout,
                cancel_token=self._loading_token,
                context=(self.renderer.network_context
                         if self.renderer else None))
            return body
        try:
            loaded = webfonts.load_web_fonts(css_texts, fetch)
        except Exception:
            loaded = 0
        if loaded:
            from . import layout as _layout
            _layout._FONT_CACHE.clear()

    def _push_layout_rects(self):
        """Feed layout geometry back to the JS engine so
        getBoundingClientRect answers real rects (document
        coordinates) from the next event handler on."""
        if self._doc is None:
            return
        # ridx -> [min_x, min_y, max_x, max_y] union of the element's boxes
        boxes = {}

        def add(ridx, x, y, w, h):
            if ridx is None:
                return
            b = boxes.get(ridx)
            if b is None:
                boxes[ridx] = [x, y, x + w, y + h]
            else:
                b[0] = min(b[0], x)
                b[1] = min(b[1], y)
                b[2] = max(b[2], x + w)
                b[3] = max(b[3], y + h)

        for o in self.layout_list:
            if isinstance(o, (BlockLayout, ImageLayout)):
                add(getattr(getattr(o, "node", None), "_ridx", None),
                    o.x, o.y, o.width, o.height)
            elif isinstance(o, TextLayout):
                # inline elements (<a>, <span>) have no box of their own —
                # they are laid out as words. Attribute each word's rect to
                # its element-ancestor chain so getBoundingClientRect
                # answers a real rect for inline links, not (0,0,0,0). Naver
                # observes inline <a> headlines with IntersectionObserver to
                # lazy-render its feed; a zero rect kept them off-screen
                # forever.
                cur = getattr(o, "node", None)
                depth = 0
                while cur is not None and depth < 6:
                    if isinstance(cur, Element):
                        add(getattr(cur, "_ridx", None),
                            o.x, o.y, o.width, o.height)
                    cur = getattr(cur, "parent", None)
                    depth += 1

        rects = [(r, float(b[0]), float(b[1]),
                  float(b[2] - b[0]), float(b[3] - b[1]))
                 for r, b in boxes.items()]
        self.renderer.set_layout_rects(rects)

    def _push_display_list(self):
        """Hand the display list to Rust once per paint change, in
        document coordinates — scroll frames then pass only offsets
        (no per-frame Python serialization, M6)."""
        if textengine.available():
            textengine.engine().set_display_list(
                [cmd.native(0) for cmd in self.display_list])

    def draw(self):
        height = self.canvas.winfo_height()
        width = self.canvas.winfo_width()
        if textengine.available():
            # Native path: Rust culls + rasterizes the stored list at
            # this scroll offset; tkinter just displays the image.
            overlay = []
            bar = self.scrollbar_rect(width, height)
            if bar:
                overlay.append((0, bar[0], bar[1], bar[2], bar[3],
                                (192, 192, 192), 0.0, 0, ""))
            ppm = textengine.engine().render_frame(
                max(width, 1), max(height, 1), (255, 255, 255),
                0.0, float(self.scroll), overlay)
            self._frame = tkinter.PhotoImage(data=ppm)
            self.canvas.delete("all")
            self.canvas.create_image(0, 0, image=self._frame, anchor="nw")
            return

        self.canvas.delete("all")
        sticky_shift = 0.0
        sticky_stack = []
        for cmd in self.display_list:
            if isinstance(cmd, DrawStickyPush):
                delta = cmd.offset(self.scroll - sticky_shift)
                sticky_stack.append(delta)
                sticky_shift += delta
                continue
            if isinstance(cmd, DrawStickyPop):
                if sticky_stack:
                    sticky_shift -= sticky_stack.pop()
                continue
            if cmd.top + sticky_shift > self.scroll + height:
                continue
            if cmd.bottom + sticky_shift < self.scroll:
                continue
            cmd.execute(self.scroll - sticky_shift, self.canvas)
        bar = self.scrollbar_rect(width, height)
        if bar:
            self.canvas.create_rectangle(
                bar[0], bar[1], bar[2], bar[3], fill="#c0c0c0", width=0)

    def scrollbar_rect(self, width, height):
        if not self.document:
            return None
        content = self.document.height + 2 * VSTEP
        if content <= height:
            return None
        bar_h = max(height * height / content, 24)
        max_scroll = content - height
        frac = self.scroll / max_scroll if max_scroll else 0
        y = frac * (height - bar_h)
        return (width - 9, y, width - 3, y + bar_h)

    # ---------- scrolling ----------

    def clamp_scroll(self):
        if not self.document:
            self.scroll = 0
            return
        height = self.canvas.winfo_height()
        max_scroll = max(self.document.height + 2 * VSTEP - height, 0)
        self.scroll = min(max(0, self.scroll), max_scroll)

    def scroll_by(self, delta):
        if self.window.focus_get() == self.url_entry:
            return
        self.scroll += delta
        self.clamp_scroll()
        self.draw()

    def scroll_to(self, y):
        self.scroll = y
        self.clamp_scroll()
        self.draw()

    def on_mousewheel(self, event):
        # a wheel inside an iframe or an overflow scroll container moves
        # that box; only when it cannot move further does the page take
        # over (scroll chaining)
        obj = self.hit_test(event.x, event.y + self.scroll) \
            if self.document else None
        ticks = int(event.delta / 120)
        frame = frames.frame_of(obj) if obj else None
        if frame is not None and frame.root is not None:
            delta = -ticks * frames.FRAME_SCROLL_STEP
            if frame.scroll_by(delta, obj.width, obj.height):
                self.repaint()
                return
        if obj is not None:
            scroller = find_scrollable(obj, -ticks * SCROLL_STEP)
            if scroller is not None and scroll_container_by(
                    scroller, -ticks * SCROLL_STEP):
                self.repaint()
                return
        self.scroll -= ticks * SCROLL_STEP
        self.clamp_scroll()
        self.draw()

    def on_configure(self, event):
        if event.width != self._layout_width and self.document:
            if self._resize_job:
                self.window.after_cancel(self._resize_job)
            self._resize_job = self.window.after(120, self.relayout)
        else:
            self.draw()

    # ---------- interaction ----------

    def hit_test(self, x, y):
        return hit_test_at(self.layout_list, x, y, self.scroll)

    def find_link(self, node):
        while node:
            if (isinstance(node, Element) and node.tag == "a"
                    and "href" in node.attributes):
                return node.attributes["href"]
            node = node.parent
        return None

    def refresh_after_js(self):
        """Re-style, re-layout and redraw after JS mutated the DOM."""
        # a click handler may have scheduled fetch/timers — settle them
        self.renderer.settle(
            timeout=self.navigation_timeout, refresh=False)
        self.nodes = self.renderer.frame()
        self._remap_marks()
        self.load_images(self.nodes, self.url, keep_cache=True)
        self._load_frames()
        for node in tree_to_list(self.nodes, []):
            if isinstance(node, Element) and node.tag == "title":
                text = " ".join(
                    c.text for c in node.children if isinstance(c, Text))
                if text.strip():
                    self.window.title(text.strip() + " - GG Browser")
                break
        self.relayout()

    def on_click(self, event):
        self.canvas.focus_set()
        obj = self.hit_test(event.x, event.y + self.scroll)
        if not obj:
            self.set_focus(None)
            return
        # clicks over an <iframe> box route into the child document
        frame = frames.frame_of(obj)
        if frame is not None and frame.root is not None:
            cx, cy = frames.child_coords(
                obj, event.x, event.y + self.scroll, frame)
            outcome = frame.click_at(cx, cy, obj.width, obj.height)
            if outcome:
                self.repaint()
            return
        clicked_ridx = self._ridx_of(obj.node)
        self.set_focus(keyboard.focus_target(obj.node))
        default_node = self._node_by_ridx(clicked_ridx) or obj.node
        return self.activate_node(default_node)

    def _node_by_ridx(self, ridx):
        if ridx is None:
            return None
        return next((node for node in tree_to_list(self.nodes, [])
                     if getattr(node, "_ridx", None) == ridx), None)

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
                    # a handler called preventDefault() (or an onclick
                    # returned false): suppress the default navigation
                    return
        href = self.find_link(default_node)
        if href:
            if href.startswith(("javascript:", "mailto:")):
                self.set_status(f"지원하지 않는 링크: {href}")
                return
            try:
                self.load(self.url.resolve(href))
            except Exception as e:
                self.set_status(f"이동 실패: {e}")
            return
        input_node = forms.find_input(default_node)
        if input_node is not None \
                and input_node.attributes.get("type", "").lower() == "file":
            self.choose_file(input_node)
            return
        checkable = forms.find_checkable(default_node)
        select = forms.find_select(default_node)
        resetter = forms.find_resetter(default_node)
        submitter = forms.find_submitter(default_node)
        if checkable is not None or select is not None \
                or resetter is not None or submitter is not None:
            self.activate_form_control(
                checkable or select or resetter or submitter)
            return

    # ---------- text input focus / typing ----------

    def _reset_interaction(self):
        """New page: no focus, no hover, no stale rule flags."""
        self.focus_node = None
        self._focus_ridx = None
        self._hover_node = None
        self._hover_ridx = None
        self._hover_marks = []
        self._hover_rules = False
        self._focus_rules = False
        self._py_rule_index = None
        self._selected_files = {}
        self._form_defaults = {}

    def _drop_focus(self):
        """The node tree was rebuilt: the focused node is orphaned."""
        if self.focus_node is not None:
            self.focus_node.is_focused = False
            self.focus_node = None

    def _remap_marks(self):
        """After a native tree rebuild, re-find the focused/hovered
        nodes by their Rust indices so the caret survives live ticks
        and hover restyles (the Rust document keeps the real state)."""
        focus_ridx = getattr(self, "_focus_ridx", None)
        hover_ridx = getattr(self, "_hover_ridx", None)
        self.focus_node = None
        self._hover_node = None
        self._hover_marks = []
        selected_files = getattr(self, "_selected_files", {})
        if focus_ridx is None and hover_ridx is None and not selected_files:
            return
        for n in tree_to_list(self.nodes, []):
            r = getattr(n, "_ridx", None)
            if r is None:
                continue
            if r == focus_ridx:
                n.is_focused = True
                self.focus_node = n
            if r == hover_ridx:
                self._hover_node = n
            if r in selected_files:
                n._selected_file = selected_files[r]

    @staticmethod
    def _ridx_of(node):
        while node is not None:
            r = getattr(node, "_ridx", None)
            if r is not None:
                return r
            node = node.parent
        return None

    def set_hover(self, el):
        """Pointer moved onto a (possibly new) element: update the
        hover chain marks and restyle if the page has :hover rules."""
        if el is self._hover_node:
            return
        self._hover_node = el
        self._hover_ridx = self._ridx_of(el)
        for n in self._hover_marks:
            n.is_hovered = False
        marks = []
        cur = el
        while cur is not None:
            if isinstance(cur, Element):
                cur.is_hovered = True
                marks.append(cur)
            cur = cur.parent
        self._hover_marks = marks
        if self._doc is not None:
            self.renderer.set_hover(self._hover_ridx)
        if self._hover_rules:
            self.restyle()

    def restyle(self):
        """Re-run style with the current hover/focus state, doing
        the least work the damage requires: paint-only changes patch
        styles in place and skip relayout entirely (M4 partial
        invalidation — the common case for hover/focus)."""
        if self._doc is not None:
            outcome = native.restyle_patch(
                self._doc, self._css_sources, self.nodes)
            if outcome == "none":
                return
            if outcome == "paint":
                # a paint-only restyle can still start a transition
                # (e.g. a:hover { color } with transition: color)
                result = self.animator.on_frame(self.nodes)
                if result.damage == "layout":
                    self.relayout()
                else:
                    self.repaint()
                    self._ensure_anim_loop()
                return
            # geometry or structure changed: re-export (styles are
            # already fresh in Rust) and relayout
            self.nodes = native.build_tree(self.renderer.export())
            self._remap_marks()
            self.load_images(self.nodes, self.url, keep_cache=True)
        elif self._py_rule_index is not None:
            style(self.nodes, self._py_rule_index)
        self.relayout()

    def set_focus(self, node):
        if self.focus_node is node:
            return
        if self.focus_node is not None:
            self.focus_node.is_focused = False
        self.focus_node = node
        self._focus_ridx = self._ridx_of(node)
        if node is not None:
            node.is_focused = True
        if self._doc is not None:
            self.renderer.set_focus(self._focus_ridx)
        if self._focus_rules:
            self.restyle()
        else:
            self.repaint()

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
        height = max(self.canvas.winfo_height(), 1)
        if top < self.scroll:
            self.scroll = top
        elif bottom > self.scroll + height:
            self.scroll = bottom - height
        self.clamp_scroll()
        self.draw()

    def focus_next(self, reverse=False):
        node = keyboard.next_focus(
            self.nodes, self.focus_node, reverse=reverse)
        self.set_focus(node)
        node = self.focus_node
        if node is not None:
            self._scroll_focus_into_view(node)
        return node

    def repaint(self):
        """Rebuild the display list without restyling or relayout —
        enough for focus caret and typed-text changes."""
        if getattr(self, "document", None) is None:
            return
        self.display_list = paint_tree(self.document, [])
        self._push_display_list()
        self.draw()

    def on_key(self, event):
        if event.keysym in ("Tab", "ISO_Left_Tab"):
            reverse = (event.keysym == "ISO_Left_Tab"
                       or bool(getattr(event, "state", 0) & 0x1))
            self.focus_next(reverse=reverse)
            return "break"
        node = self.focus_node
        if node is None:
            if event.keysym == "space":
                self.scroll_by(-600 if getattr(event, "state", 0) & 0x1
                               else 600)
                return "break"
            return None
        if event.keysym == "Escape":
            self.set_focus(None)
            return "break"
        key = {"Return": "Enter", "space": "Space", "Up": "ArrowUp",
               "Down": "ArrowDown", "Left": "ArrowLeft",
               "Right": "ArrowRight"}.get(event.keysym)
        if key is not None:
            action = keyboard.key_action(node, key)
            if action == "activate":
                self.activate_node(node)
            elif action == "submit":
                self.submit_form(node)
            elif action == "newline":
                self._append_text(node, "\n")
            elif action in ("select-next", "select-prev"):
                self.activate_form_control(
                    node, select_step=1 if action == "select-next" else -1)
            elif action == "text":
                self._append_text(node, " ")
            elif action == "scroll":
                self.scroll_by(-600 if getattr(event, "state", 0) & 0x1
                               else 600)
            else:
                return None
            return "break"
        if not keyboard.is_text_editable(node):
            return None
        value = node.attributes.get("value", "")
        if event.keysym == "BackSpace":
            if not value:
                return "break"
            value = value[:-1]
        elif event.char and event.char >= " " and event.char != "\x7f":
            value = value + event.char
        else:
            return None  # arrows etc. keep their scroll bindings
        node.attributes["value"] = value
        self.sync_attr(node, "value", value)
        self.repaint()
        return "break"

    def _append_text(self, node, text):
        value = node.attributes.get("value", "") + text
        node.attributes["value"] = value
        self.sync_attr(node, "value", value)
        self.repaint()

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
        except Exception as e:
            self.set_status(f"폼 제출 실패: {e}")

    def activate_form_control(self, control, select_step=1):
        try:
            activation = forms.activate_control(
                control, self.url, self._form_defaults,
                dispatch_event=self._dispatch_form_event,
                refresh_tree=self._fresh_form_tree,
                set_attr=(self.renderer.set_attr if self._doc else None),
                remove_attr=(self.renderer.remove_attr
                             if self._doc is not None else None),
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
        if self._doc is None:
            return self.nodes
        return native.build_tree(self.renderer.export())

    def _finish_form_activation(self, activation):
        if activation is None:
            return
        if activation.invalid:
            token = activation.invalid[0]
            first = token if isinstance(token, Element) else next(
                (node for node in tree_to_list(self.nodes, [])
                 if getattr(node, "_ridx", None) == token), None)
            if first is not None:
                self.set_focus(first)
                self.set_status(forms.validation_message(first))
            if activation.handled and self._doc is not None:
                self.refresh_after_js()
            return
        if activation.prevented:
            if activation.handled and self._doc is not None:
                self.refresh_after_js()
            return
        if activation.changed:
            if self._doc is not None:
                self.nodes = self.renderer.frame()
                self._remap_marks()
            self.relayout()
            return
        submission = activation.submission
        if submission is not None:
            self.load(
                self.url.resolve(submission.target),
                method=submission.method, body=submission.body,
                headers=submission.headers)

    def choose_file(self, node):
        """Attach a file only after an explicit native file-picker choice."""
        from tkinter import filedialog
        path = filedialog.askopenfilename(parent=self.window)
        if not path:
            return
        try:
            with open(path, "rb") as file:
                data = file.read()
            forms.attach_file(node, os.path.basename(path), data)
            ridx = getattr(node, "_ridx", None)
            if ridx is not None:
                self._selected_files[ridx] = node._selected_file
            self.sync_attr(node, "value", node.attributes["value"])
            self.repaint()
        except OSError as exc:
            self.set_status(f"파일 열기 실패: {exc}")

    def sync_attr(self, node, name, value):
        """Mirror a Python-side attribute change into the Rust DOM so
        page JS reading the input sees the typed value."""
        ridx = getattr(node, "_ridx", None)
        if self._doc is not None and ridx is not None:
            self.renderer.set_attr(ridx, name, value)

    def on_motion(self, event):
        # Hit-testing walks the layout tree; 30ms throttle keeps
        # mouse movement cheap on huge pages.
        now = time.monotonic()
        if now - getattr(self, "_last_motion", 0) < 0.03:
            return
        self._last_motion = now
        obj = self.hit_test(event.x, event.y + self.scroll)
        el = obj.node if obj else None
        while el is not None and not isinstance(el, Element):
            el = el.parent
        self.set_hover(el)
        href = self.find_link(obj.node) if obj else None
        base = self.url
        frame = frames.frame_of(obj) if obj else None
        if frame is not None and frame.root is not None:
            hit = frames.hit_frame(obj, event.x, event.y + self.scroll)
            href = frame.find_link(hit[1].node) if hit else None
            base = frame.url
        if href:
            self.canvas.config(cursor="hand2")
            self.set_status(str(base.resolve(href)) if base else href)
        else:
            self.canvas.config(cursor="")
            self.set_status("")

    def set_status(self, text):
        self.status.config(text=text)

    # ---------- entry point ----------

    def start(self, url_string=None):
        self.window.update()  # realize widgets so canvas has a size
        self.load_url_string(url_string or HOME_URL)
        try:
            self.window.mainloop()
        finally:
            self._navigation.shutdown()
            if self.frames is not None:
                self.frames.dispose()
            if self.renderer is not None:
                shutdown = getattr(self.renderer, "shutdown", None)
                if shutdown is not None:
                    shutdown()
                else:
                    self.renderer.close()
