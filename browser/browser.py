"""Browser chrome: window, toolbar, scrolling, navigation, hit-testing."""

import os
import time
import tkinter
import tkinter.font
import traceback
from concurrent.futures import ThreadPoolExecutor

from . import forms, native, net, textengine, webfonts
from .html_parser import Element, HTMLParser, Text, tree_to_list
from .css_parser import CSSParser
from .style import RuleIndex, cascade_priority, default_rules, style
from .layout import (VSTEP, BlockLayout, DocumentLayout, ImageLayout,
                     layout_tree_to_list, paint_tree)
from .pages import error_page

SCROLL_STEP = 90
HOME_URL = "about:home"


class Browser:
    def __init__(self):
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
        self.url = None
        self._doc = None            # native (Rust) document handle
        self._css_sources = []
        self._img_by_src = {}
        self.default_rules = default_rules()
        self._layout_width = 0
        self._resize_job = None

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

    def load(self, url, add_to_history=True, no_cache=False):
        self.set_status(f"로딩 중... {url}")
        self.window.update_idletasks()
        try:
            headers, body, url = net.request_text(url, no_cache=no_cache)
            # url is now the post-redirect URL: relative links, history
            # and the address bar must all use it as the base
        except Exception as e:
            body = error_page(str(url), f"{type(e).__name__}: {e}")
        try:
            self.render_page(url, body)
        except Exception:
            body = error_page(str(url), traceback.format_exc(limit=5))
            self.render_page(url, body)

        if add_to_history:
            self.history = self.history[:self.history_index + 1]
            self.history.append(url)
            self.history_index = len(self.history) - 1
        self._update_nav_buttons()

    def render_page(self, url, body):
        self.url = url
        self.url_var.set(str(url))
        self._reset_interaction()

        if native.available():
            # Rust fast path: parse + JS + cascade + style in ggcore.
            # Use gg-js so the async event loop is available.
            prev = os.environ.get("GGJS")
            if native.async_available():
                os.environ["GGJS"] = "1"
            try:
                self.nodes, self._doc, self._css_sources, js_logs = \
                    native.load_document(
                        body,
                        lambda hrefs: self.fetch_stylesheets(hrefs, url),
                        lambda srcs: self.fetch_scripts(srcs, url),
                        page_url=url)
            finally:
                if prev is None:
                    os.environ.pop("GGJS", None)
                else:
                    os.environ["GGJS"] = prev
            # drive fetch/timer-driven SPA content, then rebuild the tree
            if native.settle_async(self._doc, self._css_sources, url):
                self.nodes = native.refresh(self._doc, self._css_sources)
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
            rules = self.default_rules + self.collect_styles(self.nodes, url)
            rules = sorted(rules, key=cascade_priority)
            # Style depends only on the DOM and CSS, not on window size:
            # compute it once per page load, never on resize.
            self._py_rule_index = RuleIndex(rules)
            style(self.nodes, self._py_rule_index)
            self._hover_rules = any(
                ":hover" in repr(sel) for sel, _ in rules)
            self._focus_rules = any(
                ":focus" in repr(sel) for sel, _ in rules)

        title = "GG Browser"
        for node in tree_to_list(self.nodes, []):
            if isinstance(node, Element) and node.tag == "title":
                text = " ".join(
                    c.text for c in node.children if isinstance(c, Text))
                if text.strip():
                    title = text.strip() + " - GG Browser"
                break
        self.window.title(title)

        self.load_images(self.nodes, url)
        self._load_web_fonts(url)

        self.scroll = 0
        self.relayout()
        mode = []
        if native.available():
            mode.append("rust")
        if textengine.available():
            mode.append("raster")
        self.set_status("완료" + (f" ({'+'.join(mode)})" if mode else ""))
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

    def _start_live_loop(self):
        self._live_gen = getattr(self, "_live_gen", 0) + 1
        if (self._doc is None or not hasattr(self._doc, "tick")
                or not native.async_available()):
            return
        self._dom_version = self._doc.dom_version()
        self._live_idle = 0
        self._live_injected = 0
        import time
        self._live_last = time.monotonic()
        gen = self._live_gen
        self.window.after(self._LIVE_INTERVAL_MS,
                          lambda: self._live_tick(gen))

    def _live_tick(self, gen):
        if gen != getattr(self, "_live_gen", 0) or self._doc is None:
            return  # a newer page took over
        from . import net as _net
        import time
        try:
            # advance virtual time by real elapsed time (capped), so
            # the idle backoff doesn't slow the page's clock down
            now = time.monotonic()
            dt = min((now - self._live_last) * 1000.0, 1000.0)
            self._live_last = now
            logs, fetches = self._doc.tick(dt)
            for line in logs:
                print(f"[js live] {line}")
            for fetch_id, furl in fetches:
                try:
                    _h, body, _f = _net.request_text(
                        self.url.resolve(furl))
                    self._doc.resolve_fetch(fetch_id, 200, body)
                except Exception as e:
                    self._doc.reject_fetch(
                        fetch_id, f"{type(e).__name__}: {e}")
            # N1: scripts the page injected during this tick (the
            # injected-count cap is per page generation)
            _ran, self._live_injected = native._drain_injected_scripts(
                self._doc, self.url,
                getattr(self, "_live_injected", 0))
            version = self._doc.dom_version()
            if version != self._dom_version:
                self._dom_version = version
                self._live_idle = 0
                self.nodes = native.refresh_partial(
                    self._doc, self._css_sources, self.nodes)
                self._remap_marks()
                self.load_images(self.nodes, self.url, keep_cache=True)
                self.relayout()
            else:
                self._live_idle += 1
        except Exception as e:
            print(f"[live] tick error: {e}")
            return
        wait = (self._LIVE_IDLE_INTERVAL_MS
                if self._live_idle >= self._LIVE_IDLE_AFTER
                else self._LIVE_INTERVAL_MS)
        self.window.after(wait, lambda: self._live_tick(gen))

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
                _, data = net.request_raw(url.resolve(src))
                return data
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
        self.set_status(f"스크립트 {len(srcs)}개 병렬 로딩...")
        self.window.update_idletasks()

        def fetch(src):
            try:
                _, code = net.request(url.resolve(src))
                return code
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
                _, css = net.request(url.resolve(href))
                return css
            except Exception:
                return ""

        with ThreadPoolExecutor(max_workers=6) as pool:
            for href, css in zip(hrefs, pool.map(fetch, hrefs)):
                fetched.setdefault(href, css)
        return fetched

    def collect_styles(self, nodes, url):
        # Gather <style> contents and <link> hrefs in document order
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
        for kind, val in entries:
            css = val if kind == "inline" else fetched.get(val, "")
            if css:
                rules.extend(CSSParser(css).parse())
        return rules

    def relayout(self):
        if not hasattr(self, "nodes"):
            return
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

    def reload(self):
        if self.url:
            self.load(self.url, add_to_history=False, no_cache=True)

    def go_back(self):
        if self.history_index > 0:
            self.history_index -= 1
            self.load(self.history[self.history_index],
                      add_to_history=False)

    def go_forward(self):
        if self.history_index < len(self.history) - 1:
            self.history_index += 1
            self.load(self.history[self.history_index],
                      add_to_history=False)

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
            _, body = net.request_raw(url.resolve(u))
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
        doc = getattr(self, "_doc", None)
        if doc is None or not hasattr(doc, "set_layout_rects"):
            return
        rects = []
        seen = set()
        for o in self.layout_list:
            if not isinstance(o, (BlockLayout, ImageLayout)):
                continue  # line/text boxes share their element's node
            r = getattr(getattr(o, "node", None), "_ridx", None)
            if r is None or r in seen:
                continue
            seen.add(r)
            rects.append((r, float(o.x), float(o.y),
                          float(o.width), float(o.height)))
        doc.set_layout_rects(rects)

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
        for cmd in self.display_list:
            if cmd.top > self.scroll + height:
                continue
            if cmd.bottom < self.scroll:
                continue
            cmd.execute(self.scroll, self.canvas)
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
        self._push_scroll()

    def _push_scroll(self):
        # gBCR answers viewport-relative coords: keep the JS engine's
        # scroll offset current (cheap - two floats)
        doc = getattr(self, "_doc", None)
        if doc is not None and hasattr(doc, "set_scroll"):
            doc.set_scroll(0.0, float(self.scroll))

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
        self.scroll -= int(event.delta / 120) * SCROLL_STEP
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
        objs = [o for o in self.layout_list
                if o.x <= x < o.x + o.width
                and o.y <= y < o.y + o.height]
        return objs[-1] if objs else None

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
        native.settle_async(self._doc, self._css_sources, self.url)
        self.nodes = native.refresh(self._doc, self._css_sources)
        self._remap_marks()
        self.load_images(self.nodes, self.url, keep_cache=True)
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
        if self._doc is not None:
            target = obj.node
            while target is not None and not (
                    isinstance(target, Element)
                    and hasattr(target, "_ridx")):
                target = target.parent
            if target is not None:
                result = self._doc.dispatch_click(target._ridx)
                if len(result) == 3:
                    logs, handled, prevented = result
                else:  # pre-preventDefault ggcore wheel
                    logs, handled = result
                    prevented = handled
                for line in logs:
                    print(f"[js console] {line}")
                if handled:
                    self.refresh_after_js()
                if prevented:
                    # a handler called preventDefault() (or an onclick
                    # returned false): suppress the default navigation
                    return
        href = self.find_link(obj.node)
        if href:
            if href.startswith(("javascript:", "mailto:")):
                self.set_status(f"지원하지 않는 링크: {href}")
                return
            try:
                self.load(self.url.resolve(href))
            except Exception as e:
                self.set_status(f"이동 실패: {e}")
            return
        target = forms.find_input(obj.node)
        if target is not None and self.toggle_control(target):
            return
        self.set_focus(target)

    def toggle_control(self, node):
        """Checkbox/radio click semantics: toggle (checkbox) or select
        exclusively within the same-name group (radio). Mirrored into
        the Rust DOM so form serialization and page JS see it.
        Returns True when the click was consumed."""
        itype = node.attributes.get("type", "").strip().casefold()
        if itype == "checkbox":
            if "checked" in node.attributes:
                del node.attributes["checked"]
                self.sync_attr(node, "checked", None)
            else:
                node.attributes["checked"] = "checked"
                self.sync_attr(node, "checked", "checked")
            self.repaint()
            return True
        if itype == "radio":
            name = node.attributes.get("name", "")
            form = forms.find_form(node) or self.nodes
            if name:
                for peer in tree_to_list(form, []):
                    if (isinstance(peer, Element)
                            and peer.tag == "input"
                            and peer.attributes.get(
                                "type", "").casefold() == "radio"
                            and peer.attributes.get("name", "") == name
                            and "checked" in peer.attributes):
                        del peer.attributes["checked"]
                        self.sync_attr(peer, "checked", None)
            node.attributes["checked"] = "checked"
            self.sync_attr(node, "checked", "checked")
            self.repaint()
            return True
        return False

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
        if focus_ridx is None and hover_ridx is None:
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
        doc = getattr(self, "_doc", None)
        if doc is not None and hasattr(doc, "set_hover"):
            doc.set_hover(self._hover_ridx)
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
                self.repaint()
                return
            # geometry or structure changed: re-export (styles are
            # already fresh in Rust) and relayout
            self.nodes = native.build_tree(self._doc.export())
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
        doc = getattr(self, "_doc", None)
        if doc is not None and hasattr(doc, "set_focus"):
            doc.set_focus(self._focus_ridx)
        if self._focus_rules:
            self.restyle()
        else:
            self.repaint()

    def repaint(self):
        """Rebuild the display list without restyling or relayout —
        enough for focus caret and typed-text changes."""
        if getattr(self, "document", None) is None:
            return
        self.display_list = paint_tree(self.document, [])
        self._push_display_list()
        self.draw()

    def on_key(self, event):
        node = self.focus_node
        if node is None:
            return None
        if event.keysym == "Return":
            self.submit_form(node)
            return "break"
        if event.keysym == "Escape":
            self.set_focus(None)
            return "break"
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

    def submit_form(self, node):
        form = forms.find_form(node)
        if form is None:
            return
        href = forms.submit_href(form)
        if href is None:
            self.set_status("POST 폼은 아직 지원하지 않습니다")
            return
        try:
            self.load(self.url.resolve(href))
        except Exception as e:
            self.set_status(f"이동 실패: {e}")

    def sync_attr(self, node, name, value):
        """Mirror a Python-side attribute change into the Rust DOM so
        page JS reading the input sees the typed value."""
        doc = getattr(self, "_doc", None)
        ridx = getattr(node, "_ridx", None)
        if doc is None or ridx is None:
            return
        if value is None:
            if hasattr(doc, "remove_attr"):
                doc.remove_attr(ridx, name)
            elif hasattr(doc, "set_attr"):
                doc.set_attr(ridx, name, "")
        elif hasattr(doc, "set_attr"):
            doc.set_attr(ridx, name, value)

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
        if href:
            self.canvas.config(cursor="hand2")
            self.set_status(str(self.url.resolve(href))
                            if self.url else href)
        else:
            self.canvas.config(cursor="")
            self.set_status("")

    def set_status(self, text):
        self.status.config(text=text)

    # ---------- entry point ----------

    def start(self, url_string=None):
        self.window.update()  # realize widgets so canvas has a size
        self.load_url_string(url_string or HOME_URL)
        self.window.mainloop()
