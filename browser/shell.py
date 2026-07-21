"""Native window shell: Rust winit window, all chrome drawn by us.

No tkinter anywhere: the Rust rasterizer draws the page AND the
browser UI (toolbar, URL bar, status bar) into one frame, which is
blitted straight onto the window surface via softbuffer.
"""

import os
import time
import traceback
from concurrent.futures import ThreadPoolExecutor

from . import native, net, textengine
from .draw import scale_cmds
from .html_parser import Element, Text, tree_to_list
from .layout import (HSTEP, VSTEP, DocumentLayout, get_font,
                     layout_tree_to_list, measure, paint_tree)
from .pages import error_page

TOOLBAR_H = 44
STATUS_H = 24
SCROLL_STEP = 90
HOME_URL = "about:home"

TOOLBAR_BG = (232, 232, 232)
STATUS_BG = (240, 240, 240)
BORDER = (200, 200, 200)
INK = (40, 40, 40)


def _fetch_many(urls, base, binary=False):
    """Parallel fetch helper: {url: text-or-bytes}."""
    def fetch(u):
        try:
            if binary:
                return net.request_raw(base.resolve(u))[1]
            return net.request(base.resolve(u))[1]
        except Exception:
            return b"" if binary else ""

    out = {}
    if urls:
        with ThreadPoolExecutor(max_workers=6) as pool:
            for u, data in zip(urls, pool.map(fetch, urls)):
                out.setdefault(u, data)
    return out


class Shell:
    def __init__(self, win):
        self.win = win
        self.engine = textengine.engine()
        self.running = True
        self.dirty = True
        self.scroll = 0
        self.hscroll = 0
        self.content_width = 0
        self.content_height = 0
        self.url = None
        self.url_text = ""
        self.caret = 0
        self.url_focused = False
        self.history = []
        self.history_index = -1
        self.document = None
        self.layout_list = []
        self.display_list = []
        self._list_dirty = True
        self._pushed_scale = None
        self.nodes = None
        self._doc = None
        self._css_sources = []
        self._img_by_src = {}
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

    def load(self, url, add_to_history=True, no_cache=False):
        self.set_status(f"로딩 중... {url}")
        self.render_frame()  # immediate feedback before blocking I/O
        try:
            _headers, body, url = net.request_text(url, no_cache=no_cache)
            # url is now the post-redirect URL (base for relative links)
        except Exception as e:
            body = error_page(str(url), f"{type(e).__name__}: {e}")
        try:
            self.render_page(url, body)
        except Exception:
            self.render_page(url, error_page(
                str(url), traceback.format_exc(limit=5)))
        if add_to_history:
            self.history = self.history[:self.history_index + 1]
            self.history.append(url)
            self.history_index = len(self.history) - 1
        self.set_status("완료 (native window)")

    def render_page(self, url, body):
        self.url = url
        self.url_text = str(url)
        self.caret = len(self.url_text)
        self.url_focused = False

        # Use gg-js so the async event loop (fetch/Promise/setTimeout)
        # is available; fall back cleanly if the wheel predates it.
        prev = os.environ.get("GGJS")
        if native.async_available():
            os.environ["GGJS"] = "1"
        try:
            self.nodes, self._doc, self._css_sources, logs = \
                native.load_document(
                    body,
                    lambda hrefs: _fetch_many(hrefs, url),
                    lambda srcs: _fetch_many(srcs, url),
                    page_url=url)
        finally:
            if prev is None:
                os.environ.pop("GGJS", None)
            else:
                os.environ["GGJS"] = prev
        for line in logs:
            print(f"[js console] {line}")

        # Drive the event loop so fetch/timer-driven SPA content appears,
        # then rebuild the tree from the mutated DOM.
        if native.settle_async(self._doc, self._css_sources, url):
            self.nodes = native.refresh(self._doc, self._css_sources)

        self.apply_title()
        self.load_images(keep_cache=False)
        self.scroll = 0
        self.hscroll = 0
        self.relayout()

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
            raw = _fetch_many(srcs, self.url, binary=True)
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
            lambda urls: _fetch_many(urls, self.url, binary=True))

    def logical_size(self):
        """Window size in CSS px. Layout, hit-testing, and the UI all
        work in logical px; only the raster buffer is physical."""
        w, h = self.win.size()
        scale = self.win.scale_factor()
        return w / scale, h / scale

    def relayout(self):
        if self.nodes is None:
            return
        w, h = self.logical_size()
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
        self.clamp_scroll()
        self.dirty = True

    def refresh_after_js(self):
        # a click handler may have scheduled fetch/timers — settle them
        native.settle_async(self._doc, self._css_sources, self.url)
        self.nodes = native.refresh(self._doc, self._css_sources)
        self.load_images(keep_cache=True)
        self.apply_title()
        self.relayout()

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

    def go_home(self):
        self.load_url_string(HOME_URL)

    # ---------- events ----------

    def handle(self, kind, a, b, text):
        if kind == "close":
            self.running = False
        elif kind in ("ready", "resize"):
            self.relayout()
        elif kind == "wheel":
            self.scroll -= b
            self.hscroll -= a
            self.clamp_scroll()
            self.dirty = True
        elif kind == "mouse_move":
            self.on_motion(a, b)
        elif kind == "mouse_down" and text == "left":
            self.on_click(a, b)
        elif kind == "text":
            if self.url_focused:
                self.url_text = (self.url_text[:self.caret] + text
                                 + self.url_text[self.caret:])
                self.caret += len(text)
                self.dirty = True
        elif kind == "key":
            self.on_key(text)

    def on_key(self, name):
        if self.url_focused:
            if name == "Enter":
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

    def on_click(self, x, y):
        if y < TOOLBAR_H:
            for x1, x2, _label, action in self._buttons:
                if x1 <= x < x2:
                    action()
                    return
            bx1, _by1, bx2, _by2 = self._urlbar
            if bx1 <= x < bx2:
                self.url_focused = True
                self.caret = len(self.url_text)
                self.dirty = True
            return
        if self.url_focused:
            self.url_focused = False
            self.dirty = True
        obj = self.hit_test(x + self.hscroll, y - TOOLBAR_H + self.scroll)
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
                    # preventDefault() (or onclick returning false)
                    # suppresses the default navigation
                    return
        href = self.find_link(obj.node)
        if href and not href.startswith(("javascript:", "mailto:")):
            try:
                self.load(self.url.resolve(href))
            except Exception as e:
                self.set_status(f"이동 실패: {e}")

    def on_motion(self, x, y):
        now = time.monotonic()
        if now - self._last_motion < 0.03:
            return
        self._last_motion = now
        href = None
        if y >= TOOLBAR_H:
            obj = self.hit_test(x + self.hscroll,
                                y - TOOLBAR_H + self.scroll)
            href = self.find_link(obj.node) if obj else None
        self.win.set_cursor_pointer(href is not None)
        status = str(self.url.resolve(href)) if href and self.url else ""
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

    while shell.running:
        for kind, a, b, text in win.pump(16):
            shell.handle(kind, a, b, text)
            if kind == "ready" and not loaded:
                loaded = True
                shell.load_url_string(pending)
        if shell.dirty:
            shell.render_frame()
