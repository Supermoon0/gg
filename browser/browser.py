"""Browser chrome: window, toolbar, scrolling, navigation, hit-testing."""

import os
import time
import tkinter
import tkinter.font
import traceback
from concurrent.futures import ThreadPoolExecutor

from . import native, net, textengine
from .html_parser import Element, HTMLParser, Text, tree_to_list
from .css_parser import CSSParser
from .style import RuleIndex, cascade_priority, default_rules, style
from .layout import (VSTEP, DocumentLayout, layout_tree_to_list, paint_tree)
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
                        lambda srcs: self.fetch_scripts(srcs, url))
            finally:
                if prev is None:
                    os.environ.pop("GGJS", None)
                else:
                    os.environ["GGJS"] = prev
            # drive fetch/timer-driven SPA content, then rebuild the tree
            if native.settle_async(self._doc, self._css_sources, url):
                self.nodes = native.refresh(self._doc, self._css_sources)
            for line in js_logs:
                print(f"[js console] {line}")
            if js_logs:
                self.set_status(f"JS 콘솔 {len(js_logs)}줄 (터미널 참고)")
        else:
            # Pure-Python fallback
            self._doc = None
            self.nodes = HTMLParser(body).parse()
            rules = self.default_rules + self.collect_styles(self.nodes, url)
            rules = sorted(rules, key=cascade_priority)
            # Style depends only on the DOM and CSS, not on window size:
            # compute it once per page load, never on resize.
            style(self.nodes, RuleIndex(rules))

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

        self.scroll = 0
        self.relayout()
        mode = []
        if native.available():
            mode.append("rust")
        if textengine.available():
            mode.append("raster")
        self.set_status("완료" + (f" ({'+'.join(mode)})" if mode else ""))

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

    def draw(self):
        height = self.canvas.winfo_height()
        width = self.canvas.winfo_width()
        if textengine.available():
            # Native path: Rust rasterizes the frame; tkinter just
            # displays the resulting image.
            cmds = []
            for cmd in self.display_list:
                if cmd.top > self.scroll + height:
                    continue
                if cmd.bottom < self.scroll:
                    continue
                cmds.append(cmd.native(self.scroll))
            bar = self.scrollbar_rect(width, height)
            if bar:
                cmds.append((0, bar[0], bar[1], bar[2], bar[3],
                             (192, 192, 192), 0.0, 0, ""))
            ppm = textengine.engine().render(
                max(width, 1), max(height, 1), (255, 255, 255), cmds)
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

    def on_motion(self, event):
        # Hit-testing walks the layout tree; 30ms throttle keeps
        # mouse movement cheap on huge pages.
        now = time.monotonic()
        if now - getattr(self, "_last_motion", 0) < 0.03:
            return
        self._last_motion = now
        obj = self.hit_test(event.x, event.y + self.scroll)
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
