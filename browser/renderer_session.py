"""Renderer lifecycle contract and its current in-process implementation.

The class owns one live ``ggcore.Doc`` plus its resource loading and event-loop
plumbing.  GUI shells and the headless driver can use this contract now; a
future remote implementation will keep the same operations behind validated
IPC without moving DOM or gg-js out of the renderer process.
"""

from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
import json
import os
import time
from typing import Protocol, runtime_checkable

from . import native, net
from .network_backend import default_network_backend


@dataclass(frozen=True)
class RendererUpdate:
    logs: tuple[str, ...] = ()
    dom_changed: bool = False
    fetch_count: int = 0
    pending_work: bool = False


@dataclass(frozen=True)
class RendererState:
    document_url: str | None
    dom_version: int | None
    pending_work: bool
    console_count: int


@runtime_checkable
class RendererSession(Protocol):
    def commit(self, url, body, **kwargs): ...

    def tick(self, dt_ms=None): ...

    def settle(self, **kwargs): ...

    def refresh(self, viewport_width=None): ...

    def frame(self, viewport_width=None): ...

    def run(self, sources): ...

    def dispatch_click(self, node_idx): ...

    def dispatch_event(self, *args): ...

    def query(self, selector, first=False): ...

    def snapshot(self): ...

    def export(self): ...

    def state(self): ...

    def set_attr(self, node_idx, name, value): ...

    def remove_attr(self, node_idx, name): ...

    def set_focus(self, node_idx): ...

    def set_hover(self, node_idx): ...

    def set_layout_rects(self, rects): ...

    def set_scroll_state(self, state): ...

    def take_scroll_writes(self): ...

    def take_scroll_into_view(self): ...

    def set_frame_graph(self, frames): ...

    def take_frame_writes(self): ...

    def deliver_message(self, data_json, origin, source_handle): ...

    def set_frame_document(self, node, handle, url, rows): ...

    def take_frame_dom_writes(self): ...

    def set_text_content(self, node_idx, text): ...

    def restyle_diff(self): ...

    def close(self): ...


class LocalRendererSession:
    """One renderer document hosted in the current process."""

    def __init__(self, network_backend=None, *, run_scripts=True,
                 timeout=net.DEFAULT_TIMEOUT, js_budget=8.0):
        if not native.available():
            raise RuntimeError("native ggcore wheel is required")
        self.network = network_backend or default_network_backend()
        self.run_scripts = bool(run_scripts)
        self.timeout = timeout
        self.js_budget = js_budget
        self.url = None
        self.root = None
        self.doc = None
        self.css_sources = []
        self.console = []
        self.viewport_width = 1280.0
        self.cancel_token = None
        self.network_context = None
        self._dom_version = None

    def commit(self, url, body, *, viewport_width=1280.0, timings=None,
               cancel_token=None, network_context=None, framed=False):
        if self.doc is not None or self.network_context is not None:
            self.close()
        self.url = url
        self.viewport_width = float(viewport_width)
        self.cancel_token = cancel_token
        self.network_context = (
            network_context or self.network.bind_context(url))
        fetch_js = self._fetch_text_many if self.run_scripts else None
        root, doc, css_sources, logs = native.load_document(
            body, self._fetch_text_many, fetch_js,
            js_budget=self.js_budget, page_url=url,
            viewport_width=self.viewport_width, timings=timings,
            network_backend=self.network,
            network_context=self.network_context,
            network_timeout=self.timeout,
            cancel_token=cancel_token, framed=framed)
        self.root = root
        self.doc = doc
        self.css_sources = list(css_sources)
        self.console = list(logs)
        self._dom_version = (doc.dom_version()
                             if hasattr(doc, "dom_version") else None)
        return root, doc, self.css_sources, list(logs)

    def _fetch_text_many(self, urls):
        return self._fetch_many(urls, binary=False)

    def fetch_binary_many(self, urls):
        return self._fetch_many(urls, binary=True)

    def _fetch_many(self, urls, *, binary):
        out = {}
        if not urls:
            return out

        def fetch(value):
            try:
                resolved = self.url.resolve(value)
                common = {
                    "site_for_cookies": self.url,
                    "top_level_navigation": False,
                    "timeout": self.timeout,
                    "cancel_token": self.cancel_token,
                    "context": self.network_context,
                }
                if binary:
                    return self.network.request_raw(resolved, **common)[1]
                return self.network.request(resolved, **common)[1]
            except net.RequestCancelled:
                raise
            except Exception:
                return b"" if binary else ""

        with ThreadPoolExecutor(max_workers=6) as pool:
            for value, data in zip(urls, pool.map(fetch, urls)):
                out.setdefault(value, data)
        return out

    def sync_cookie_writes(self):
        return native.sync_cookie_writes(
            self.doc, self.url, network_backend=self.network,
            network_context=self.network_context)

    def service_fetch(self, request):
        return native.service_script_fetch(
            self.doc, self.url, request, network_timeout=self.timeout,
            cancel_token=self.cancel_token, network_backend=self.network,
            network_context=self.network_context)

    def tick(self, dt_ms=None):
        if self.doc is None or not native.async_available():
            return RendererUpdate()
        logs, requests = native.pump_script_requests(self.doc, dt_ms)
        self.sync_cookie_writes()
        for request in requests:
            self.service_fetch(request)
        version = (self.doc.dom_version()
                   if hasattr(self.doc, "dom_version") else None)
        changed = version != self._dom_version
        if changed:
            self._dom_version = version
        self.console.extend(logs)
        pending = (self.doc.has_pending_work()
                   if hasattr(self.doc, "has_pending_work") else False)
        return RendererUpdate(
            tuple(logs), changed, len(requests), bool(pending))

    def settle(self, *, timeout=8.0, max_rounds=2000,
               refresh=True):
        if self.doc is None or not native.async_available():
            return RendererUpdate()
        deadline = time.monotonic() + timeout
        all_logs = []
        changed = False
        fetch_count = 0
        pending = False
        for round_no in range(max_rounds):
            # Drain due work first (dt=None leaves the virtual clock
            # alone), then — once the fetch/microtask stream quiets but
            # timers remain — advance the clock so setTimeout(fn, >0)
            # eventually fires. Without this a page whose data layer
            # retries or debounces on a timer never settles: the shell
            # ticks real dt each frame, headless settle must too.
            update = self.tick()
            all_logs.extend(update.logs)
            changed = changed or update.dom_changed
            fetch_count += update.fetch_count
            pending = update.pending_work
            if update.fetch_count:
                continue
            if not pending:
                break
            step = self.tick(dt_ms=25.0)
            all_logs.extend(step.logs)
            changed = changed or step.dom_changed
            fetch_count += step.fetch_count
            pending = step.pending_work
            if not pending and not step.fetch_count:
                break
            if time.monotonic() > deadline:
                timeout_log = "[renderer] settle timed out"
                all_logs.append(timeout_log)
                self.console.append(timeout_log)
                break
        self.sync_cookie_writes()
        if changed and refresh:
            self.refresh()
        return RendererUpdate(
            tuple(all_logs), changed, fetch_count, bool(pending))

    def refresh(self, viewport_width=None):
        if self.doc is None:
            return None
        if viewport_width is not None:
            self.viewport_width = float(viewport_width)
        self.root = native.refresh(
            self.doc, self.css_sources, self.viewport_width)
        return self.root

    def frame(self, viewport_width=None):
        """Return the current local frame tree after style recomputation."""
        return self.refresh(viewport_width)

    def run(self, sources):
        if self.doc is None:
            raise RuntimeError("renderer has no committed document")
        logs = list(self.doc.run_scripts(list(sources)))
        self.sync_cookie_writes()
        self.console.extend(logs)
        version = (self.doc.dom_version()
                   if hasattr(self.doc, "dom_version") else None)
        if version != self._dom_version:
            self._dom_version = version
        return logs

    def query(self, selector, first=False):
        if self.doc is None or not hasattr(self.doc, "query"):
            return []
        return list(self.doc.query(selector, first))

    def snapshot(self):
        if self.doc is None or not hasattr(self.doc, "snapshot"):
            return []
        return list(self.doc.snapshot())

    def export(self):
        return list(self.doc.export()) if self.doc is not None else []

    def dispatch_click(self, node_idx):
        return self.doc.dispatch_click(node_idx)

    def dispatch_event(self, *args):
        return self.doc.dispatch_event(*args)

    def set_attr(self, node_idx, name, value):
        return self.doc.set_attr(node_idx, name, value)

    def remove_attr(self, node_idx, name):
        if hasattr(self.doc, "remove_attr"):
            return self.doc.remove_attr(node_idx, name)
        return None

    def set_focus(self, node_idx):
        if hasattr(self.doc, "set_focus"):
            return self.doc.set_focus(node_idx)
        return None

    def set_hover(self, node_idx):
        if hasattr(self.doc, "set_hover"):
            return self.doc.set_hover(node_idx)
        return None

    def set_layout_rects(self, rects):
        if hasattr(self.doc, "set_layout_rects"):
            return self.doc.set_layout_rects(rects)
        return None

    def set_scroll_state(self, state):
        if hasattr(self.doc, "set_scroll_state"):
            return self.doc.set_scroll_state(state)
        return None

    def take_scroll_writes(self):
        if self.doc is not None and hasattr(self.doc, "take_scroll_writes"):
            return list(self.doc.take_scroll_writes())
        return []

    def take_scroll_into_view(self):
        if self.doc is not None \
                and hasattr(self.doc, "take_scroll_into_view"):
            return list(self.doc.take_scroll_into_view())
        return []

    def set_frame_graph(self, frames):
        if self.doc is not None and hasattr(self.doc, "set_frame_graph"):
            return self.doc.set_frame_graph([tuple(r) for r in frames])
        return None

    def take_frame_writes(self):
        if self.doc is not None and hasattr(self.doc, "take_frame_writes"):
            return list(self.doc.take_frame_writes())
        return []

    def set_frame_document(self, node, handle, url, rows):
        if self.doc is None or not hasattr(self.doc, "set_frame_document"):
            return None
        return self.doc.set_frame_document(
            int(node), int(handle), str(url), [tuple(r) for r in rows])

    def take_frame_dom_writes(self):
        if self.doc is not None \
                and hasattr(self.doc, "take_frame_dom_writes"):
            return list(self.doc.take_frame_dom_writes())
        return []

    def set_text_content(self, node_idx, text):
        if self.doc is None or not hasattr(self.doc, "set_text_content"):
            return None
        return self.doc.set_text_content(int(node_idx), str(text))

    def deliver_message(self, data_json, origin, source_handle):
        if self.doc is None or not hasattr(self.doc, "deliver_message"):
            return []
        logs = list(self.doc.deliver_message(
            str(data_json), str(origin), int(source_handle)))
        # Delivery is a macrotask, so drive one pump here rather than
        # waiting for the next animation frame. Doing it through the
        # same path `tick` uses means a handler that calls fetch() (or
        # replies with postMessage) is serviced in this same turn,
        # which is what lets a request/response handshake complete
        # without a repaint in between.
        if native.async_available():
            more, requests = native.pump_script_requests(self.doc, None)
            self.sync_cookie_writes()
            for request in requests:
                self.service_fetch(request)
            logs.extend(more)
        # `_dom_version` is deliberately NOT advanced here: a handler
        # that rewrote the DOM has to look like a change to the next
        # tick, which is what makes the shell re-export and repaint.
        self.console.extend(logs)
        return logs

    def restyle_diff(self):
        if not hasattr(self.doc, "restyle_diff"):
            return 3, []
        return self.doc.restyle_diff(self.css_sources)

    def has_pending_work(self):
        return bool(self.doc and self.doc.has_pending_work())

    def state(self):
        pending = (self.has_pending_work()
                   if self.doc is not None
                   and hasattr(self.doc, "has_pending_work") else False)
        return RendererState(
            str(self.url) if self.url is not None else None,
            self._dom_version,
            bool(pending),
            len(self.console))

    def close(self):
        token = self.cancel_token
        if token is not None:
            try:
                self.network.cancel(token)
            except Exception:
                pass
        context = self.network_context
        if context is not None:
            try:
                self.network.drop_context(context)
            except Exception:
                pass
        self.cancel_token = None
        self.network_context = None
        self.root = None
        self.doc = None
        self.css_sources = []
        self._dom_version = None


class _RemoteDocProxy:
    """Compatibility facade while GUI layout remains in the browser process."""

    def __init__(self, session):
        self._session = session

    def export(self):
        return self._session.export()

    def query(self, selector, first=False):
        return self._session.query(selector, first)

    def snapshot(self):
        return self._session.snapshot()

    def run_scripts(self, sources):
        return self._session.run(sources)

    def dispatch_click(self, node_idx):
        return self._session.dispatch_click(node_idx)

    def dispatch_event(self, *args):
        return self._session.dispatch_event(*args)

    def set_attr(self, node_idx, name, value):
        return self._session.set_attr(node_idx, name, value)

    def remove_attr(self, node_idx, name):
        return self._session.remove_attr(node_idx, name)

    def set_focus(self, node_idx):
        return self._session.set_focus(node_idx)

    def set_hover(self, node_idx):
        return self._session.set_hover(node_idx)

    def set_layout_rects(self, rects):
        return self._session.set_layout_rects(rects)

    def set_scroll_state(self, state):
        return self._session.set_scroll_state(state)

    def take_scroll_writes(self):
        return self._session.take_scroll_writes()

    def take_scroll_into_view(self):
        return self._session.take_scroll_into_view()

    def set_frame_graph(self, frames):
        return self._session.set_frame_graph(frames)

    def take_frame_writes(self):
        return self._session.take_frame_writes()

    def deliver_message(self, data_json, origin, source_handle):
        return self._session.deliver_message(
            data_json, origin, source_handle)

    def set_frame_document(self, node, handle, url, rows):
        return self._session.set_frame_document(node, handle, url, rows)

    def take_frame_dom_writes(self):
        return self._session.take_frame_dom_writes()

    def set_text_content(self, node_idx, text):
        return self._session.set_text_content(node_idx, text)

    def restyle_diff(self, _css_sources=None):
        return self._session.restyle_diff()

    def compute_styles(self, _css_sources=None, viewport_width=None):
        self._session.frame(viewport_width)

    def dom_version(self):
        return self._session.state().dom_version

    def has_pending_work(self):
        return self._session.has_pending_work()

    def tick(self, dt_ms=None):
        return self._session.tick(dt_ms)


class RemoteRendererSession:
    """RendererSession proxy backed by a crash-contained child process."""

    def __init__(self, network_backend=None, *, run_scripts=True,
                 timeout=net.DEFAULT_TIMEOUT, js_budget=8.0):
        from .process.browser_host import RendererProcessHost

        if not native.available():
            raise RuntimeError("native ggcore wheel is required")
        self.network = network_backend or default_network_backend()
        self.run_scripts = bool(run_scripts)
        self.timeout = float(timeout)
        self.js_budget = float(js_budget)
        self.host = RendererProcessHost(
            self.network, request_timeout=max(self.timeout, self.js_budget + 2.0))
        self.url = None
        self.root = None
        self.doc = None
        self.css_sources = []
        self.console = []
        self.viewport_width = 1280.0
        self.cancel_token = None
        self.network_context = None

    @property
    def pid(self):
        return self.host.pid

    @property
    def alive(self):
        return self.host.alive

    def commit(self, url, body, *, viewport_width=1280.0, timings=None,
               cancel_token=None, network_context=None, framed=False):
        from .ipc.blobs import BlobStore

        if network_context is not None:
            raise ValueError("remote renderer contexts are browser-broker owned")
        self.url = url if isinstance(url, net.URL) else net.URL(str(url))
        self.viewport_width = float(viewport_width)
        self.cancel_token = cancel_token
        blobs = BlobStore()
        try:
            response = self.host.call(
                "renderer.commit", {
                    "url": str(self.url),
                    "body_blob": blobs.pack(
                        str(body).encode("utf-8"), "text/html; charset=utf-8"),
                    "viewport_width": self.viewport_width,
                    "run_scripts": self.run_scripts,
                    "timeout": self.timeout,
                    "js_budget": self.js_budget,
                    "framed": bool(framed),
                }, timeout=max(self.timeout, self.js_budget + 2.0),
                advance_generation=True, cancel_token=cancel_token)
        finally:
            blobs.release_all()
        self.root = native.build_tree(response["export"])
        self.doc = _RemoteDocProxy(self)
        self.css_sources = list(response.get("css_sources", []))
        logs = list(response.get("logs", []))
        self.console = logs[:]
        if timings is not None:
            timings.update(response.get("timings", {}))
        return self.root, self.doc, self.css_sources, logs

    def tick(self, dt_ms=None):
        response = self.host.call(
            "renderer.tick", {"dt_ms": dt_ms}, timeout=self.timeout)
        update = RendererUpdate(
            tuple(response.get("logs", [])),
            bool(response.get("dom_changed", False)),
            int(response.get("fetch_count", 0)),
            bool(response.get("pending_work", False)))
        self.console.extend(update.logs)
        return update

    def settle(self, *, timeout=8.0, max_rounds=2000, refresh=True):
        response = self.host.call(
            "renderer.settle", {
                "timeout": timeout, "max_rounds": max_rounds,
                "refresh": refresh,
            }, timeout=max(float(timeout) + 1.0, self.timeout))
        update = RendererUpdate(
            tuple(response.get("logs", [])),
            bool(response.get("dom_changed", False)),
            int(response.get("fetch_count", 0)),
            bool(response.get("pending_work", False)))
        self.console.extend(update.logs)
        return update

    def refresh(self, viewport_width=None):
        return self.frame(viewport_width)

    def frame(self, viewport_width=None):
        if viewport_width is not None:
            self.viewport_width = float(viewport_width)
        response = self.host.call(
            "renderer.frame", {"viewport_width": self.viewport_width},
            timeout=self.timeout)
        self.root = native.build_tree(response["export"])
        return self.root

    def run(self, sources):
        response = self.host.call(
            "renderer.run", {"sources": list(sources)}, timeout=self.timeout)
        logs = list(response.get("logs", []))
        self.console.extend(logs)
        return logs

    def query(self, selector, first=False):
        response = self.host.call(
            "renderer.query", {"selector": str(selector), "first": bool(first)},
            timeout=self.timeout)
        return list(response.get("rows", []))

    def snapshot(self):
        return list(self.host.call(
            "renderer.snapshot", timeout=self.timeout).get("rows", []))

    def export(self):
        return list(self.host.call(
            "renderer.export", timeout=self.timeout).get("rows", []))

    def dispatch_click(self, node_idx):
        return tuple(self.host.call(
            "renderer.click", {"node_idx": int(node_idx)},
            timeout=self.timeout).get("result", []))

    def dispatch_event(self, *args):
        return tuple(self.host.call(
            "renderer.event", {"args": list(args)},
            timeout=self.timeout).get("result", []))

    def set_attr(self, node_idx, name, value):
        return self.host.call("renderer.set_attr", {
            "node_idx": int(node_idx), "name": str(name), "value": str(value)},
            timeout=self.timeout).get("result")

    def remove_attr(self, node_idx, name):
        return self.host.call("renderer.remove_attr", {
            "node_idx": int(node_idx), "name": str(name)},
            timeout=self.timeout).get("result")

    def set_focus(self, node_idx):
        return self.host.call(
            "renderer.set_focus", {"node_idx": node_idx},
            timeout=self.timeout).get("result")

    def set_hover(self, node_idx):
        return self.host.call(
            "renderer.set_hover", {"node_idx": node_idx},
            timeout=self.timeout).get("result")

    def set_layout_rects(self, rects):
        return self.host.call(
            "renderer.set_layout_rects", {"rects": list(rects)},
            timeout=self.timeout).get("result")

    def set_scroll_state(self, state):
        return self.host.call(
            "renderer.set_scroll_state", {"state": list(state)},
            timeout=self.timeout).get("result")

    def take_scroll_writes(self):
        return self.host.call(
            "renderer.take_scroll_writes",
            timeout=self.timeout).get("writes", [])

    def take_scroll_into_view(self):
        return self.host.call(
            "renderer.take_scroll_into_view",
            timeout=self.timeout).get("nodes", [])

    def set_frame_graph(self, frames):
        return self.host.call(
            "renderer.set_frame_graph", {"frames": [list(r) for r in frames]},
            timeout=self.timeout).get("result")

    def take_frame_writes(self):
        return self.host.call(
            "renderer.take_frame_writes",
            timeout=self.timeout).get("messages", [])

    def deliver_message(self, data_json, origin, source_handle):
        return self.host.call(
            "renderer.deliver_message", {
                "data": str(data_json), "origin": str(origin),
                "source": int(source_handle),
            }, timeout=self.timeout).get("logs", [])

    def set_frame_document(self, node, handle, url, rows):
        from .ipc.blobs import BlobStore

        # a mirror can be thousands of nodes; MAX_CONTROL_BYTES is 1MiB
        blobs = BlobStore()
        try:
            return self.host.call(
                "renderer.set_frame_document", {
                    "node": int(node), "handle": int(handle),
                    "url": str(url),
                    "rows_blob": blobs.pack(
                        json.dumps(rows).encode("utf-8"),
                        "application/json"),
                }, timeout=self.timeout).get("result")
        finally:
            blobs.release_all()

    def take_frame_dom_writes(self):
        return self.host.call(
            "renderer.take_frame_dom_writes",
            timeout=self.timeout).get("writes", [])

    def set_text_content(self, node_idx, text):
        return self.host.call(
            "renderer.set_text_content",
            {"node_idx": int(node_idx), "text": str(text)},
            timeout=self.timeout).get("result")

    def restyle_diff(self):
        response = self.host.call("renderer.restyle_diff", timeout=self.timeout)
        return int(response.get("outcome", 3)), response.get("patches", [])

    def has_pending_work(self):
        return bool(self.host.call(
            "renderer.has_pending", timeout=self.timeout).get("pending", False))

    def state(self):
        response = self.host.call("renderer.state", timeout=self.timeout)
        return RendererState(
            response.get("document_url"), response.get("dom_version"),
            bool(response.get("pending_work", False)),
            int(response.get("console_count", 0)))

    def close(self):
        if self.host.alive and self.doc is not None:
            self.host.call("renderer.close", timeout=2.0)
        self.root = None
        self.doc = None
        self.css_sources = []
        self.cancel_token = None

    def shutdown(self):
        self.host.shutdown()
        self.root = None
        self.doc = None

    def test_crash(self):
        return self.host.test_crash()

    def test_hang(self, timeout=0.1):
        return self.host.test_hang(timeout)

    def test_stale(self):
        return self.host.test_stale()

    def __del__(self):
        try:
            self.host.shutdown(force=True)
        except Exception:
            pass


def create_renderer_session(network_backend=None, *, process_model=None,
                            run_scripts=True, timeout=net.DEFAULT_TIMEOUT,
                            js_budget=8.0):
    model = (process_model or os.environ.get("GG_PROCESS_MODEL", "local")).casefold()
    if model == "local":
        return LocalRendererSession(
            network_backend, run_scripts=run_scripts,
            timeout=timeout, js_budget=js_budget)
    if model == "isolated":
        return RemoteRendererSession(
            network_backend, run_scripts=run_scripts,
            timeout=timeout, js_budget=js_budget)
    raise ValueError(f"unknown GG_PROCESS_MODEL: {model}")
