"""Renderer child entry point and browser-side process/broker host."""

from __future__ import annotations

from dataclasses import asdict
import json
import multiprocessing
import os
import secrets
import threading
import time
import uuid

from .. import native, net
from ..ipc.blobs import BlobStore, read_blob
from ..ipc.channel import JsonChannel
from ..ipc.protocol import BUILD_ID, MAX_IN_FLIGHT, ProtocolError
from ..renderer_session import LocalRendererSession
from ..security import ScriptFetchResponse
from .sandbox import apply_renderer_sandbox


class RendererProcessError(RuntimeError):
    pass


class RendererCrashed(RendererProcessError):
    pass


class RendererHung(RendererProcessError):
    pass


class RemoteRendererError(RendererProcessError):
    pass


class _BrokerCancelToken:
    def __init__(self):
        self.cancelled = False

    def cancel(self):
        old = self.cancelled
        self.cancelled = True
        return not old

    def check(self):
        if self.cancelled:
            raise net.RequestCancelled("request cancelled")


class _BrokerNetworkBackend:
    """Renderer-side proxy; all privileged network work stays in browser."""

    def __init__(self, channel, state):
        self.channel = channel
        self.state = state
        self._rpc_lock = threading.Lock()

    def _rpc(self, message_type, payload):
        # Resource loading fans out across Python worker threads. Keep the
        # single message pipe ordered until request-id routing becomes its
        # own reader loop; otherwise two threads can consume each other's
        # replies and leave a broker response in the renderer command queue.
        with self._rpc_lock:
            payload = dict(payload)
            payload["deadline_ms"] = int(
                (time.monotonic() + self.state.get("timeout", 15.0)) * 1000)
            request = self.channel.send(
                message_type, payload=payload,
                generation=self.state["generation"],
                document_token=self.state["document_token"])
            response = self.channel.recv()
            if response["reply_to"] != request["msg_id"]:
                raise ProtocolError("unexpected_reply", "broker reply did not match request")
            if response["generation"] != self.state["generation"]:
                raise ProtocolError("stale_generation", "stale broker response")
            if response["type"] == "broker.error":
                error = response["payload"]
                raise RemoteRendererError(
                    f"{error.get('name', 'NetworkError')}: {error.get('message', '')}")
            if response["type"] != "broker.response":
                raise ProtocolError("unexpected_type", "expected broker.response")
            return response["payload"]

    @staticmethod
    def _kwargs(kwargs):
        context = kwargs.pop("context", None)
        kwargs.pop("cancel_token", None)
        clean = {
            key: value for key, value in kwargs.items()
            if value is None or isinstance(value, (str, int, float, bool, dict, list))
        }
        if context is not None:
            clean["context_id"] = context.get("context_id")
        return clean

    def bind_context(self, document_url, *, profile_id="default"):
        return self._rpc("broker.bind_context", {
            "document_url": str(document_url), "profile_id": str(profile_id)})["context"]

    def drop_context(self, context):
        return self._rpc("broker.drop_context", {
            "context_id": context.get("context_id")})["dropped"]

    def new_cancel_token(self):
        return _BrokerCancelToken()

    def cancel(self, token):
        return token.cancel()

    def request_text(self, url, **kwargs):
        payload = self._rpc("broker.request_text", {
            "url": str(url), "kwargs": self._kwargs(kwargs)})
        return (payload["headers"],
                read_blob(payload["body_blob"]).decode("utf-8", errors="replace"),
                net.URL(payload["final_url"]))

    def request(self, url, **kwargs):
        payload = self._rpc("broker.request", {
            "url": str(url), "kwargs": self._kwargs(kwargs)})
        return (payload["headers"],
                read_blob(payload["body_blob"]).decode("utf-8", errors="replace"))

    def request_raw(self, url, **kwargs):
        payload = self._rpc("broker.request_raw", {
            "url": str(url), "kwargs": self._kwargs(kwargs)})
        return payload["headers"], read_blob(payload["body_blob"])

    def perform_script_fetch(self, base_url, request, **kwargs):
        payload = self._rpc("broker.script_fetch", {
            "base_url": str(base_url), "request": request,
            "kwargs": self._kwargs(kwargs)})
        return ScriptFetchResponse(
            payload["status"], payload["headers"],
            read_blob(payload["body_blob"]).decode("utf-8", errors="replace"),
            net.URL(payload["final_url"]), payload.get("opaque", False))

    def cookies_for(self, url, **kwargs):
        payload = self._rpc("broker.cookies_for", {
            "url": str(url), "kwargs": self._kwargs(kwargs)})
        return payload["value"]

    def set_cookie_from_js(self, url, value, **kwargs):
        payload = self._rpc("broker.cookie_set", {
            "url": str(url), "value": str(value),
            "kwargs": self._kwargs(kwargs)})
        return payload["accepted"]


def _worker_result(channel, request, payload):
    channel.send(
        "renderer.response", payload=payload,
        reply_to=request["msg_id"], generation=request["generation"],
        document_token=request["document_token"])


def _worker_error(channel, request, exc):
    channel.send(
        "renderer.error",
        payload={"name": type(exc).__name__, "message": str(exc)[:4096]},
        reply_to=request["msg_id"], generation=request["generation"],
        document_token=request["document_token"])


def renderer_worker(connection, renderer_id):
    """Spawn target. Page-controlled data crosses only JsonChannel bytes."""
    # quota + privilege reduction BEFORE any page bytes are parsed
    sandbox_report = apply_renderer_sandbox()
    channel = JsonChannel(connection, renderer_id)
    session = None
    state = {"generation": 0, "document_token": "", "timeout": 15.0}
    try:
        hello = channel.recv()
        if hello["type"] != "hello":
            raise ProtocolError("handshake_required", "first message must be hello")
        hp = hello["payload"]
        if hp.get("role") != "browser" or hp.get("build_id") != BUILD_ID \
                or not isinstance(hp.get("nonce"), str):
            raise ProtocolError("bad_handshake", "handshake identity mismatch")
        channel.send(
            "hello_ack", reply_to=hello["msg_id"],
            payload={"role": "renderer", "build_id": BUILD_ID,
                     "nonce": hp["nonce"], "pid": os.getpid(),
                     "sandbox": sandbox_report})

        backend = _BrokerNetworkBackend(channel, state)
        while True:
            try:
                request = channel.recv()
            except EOFError:
                break
            generation = request["generation"]
            if generation < state["generation"]:
                _worker_error(
                    channel, request,
                    ProtocolError("stale_generation", "request generation is stale"))
                continue
            deadline_ms = request["payload"].get("deadline_ms")
            if deadline_ms is not None and time.monotonic() * 1000 > deadline_ms:
                _worker_error(channel, request, TimeoutError("request deadline expired"))
                continue
            kind = request["type"]
            try:
                if kind == "renderer.commit":
                    if generation <= state["generation"] and session is not None:
                        raise ProtocolError(
                            "stale_generation", "commit generation must advance")
                    if session is not None:
                        session.close()
                    state["generation"] = generation
                    state["document_token"] = request["document_token"]
                    p = request["payload"]
                    state["timeout"] = float(p.get("timeout", 15.0))
                    session = LocalRendererSession(
                        backend, run_scripts=p.get("run_scripts", True),
                        timeout=float(p.get("timeout", 15.0)),
                        js_budget=float(p.get("js_budget", 3.0)))
                    framed = bool(p.get("framed", False))
                    timings = {}
                    body = read_blob(p["body_blob"]).decode("utf-8", errors="replace")
                    root, _doc, css, logs = session.commit(
                        net.URL(p["url"]), body,
                        viewport_width=float(p.get("viewport_width", 1280.0)),
                        timings=timings, framed=framed,
                        cancel_token=backend.new_cancel_token())
                    _worker_result(channel, request, {
                        "export": session.export(), "css_sources": css,
                        "logs": logs, "timings": timings,
                        "root_present": root is not None})
                elif kind == "renderer.tick":
                    _worker_result(
                        channel, request,
                        asdict(session.tick(request["payload"].get("dt_ms"))))
                elif kind == "renderer.settle":
                    p = request["payload"]
                    _worker_result(channel, request, asdict(session.settle(
                        timeout=float(p.get("timeout", 8.0)),
                        max_rounds=int(p.get("max_rounds", 2000)),
                        refresh=bool(p.get("refresh", True)))))
                elif kind == "renderer.frame":
                    session.frame(request["payload"].get("viewport_width"))
                    _worker_result(channel, request, {"export": session.export()})
                elif kind == "renderer.run":
                    _worker_result(channel, request, {
                        "logs": session.run(request["payload"].get("sources", []))})
                elif kind == "renderer.query":
                    p = request["payload"]
                    _worker_result(channel, request, {"rows": session.query(
                        p.get("selector", ""), bool(p.get("first", False)))})
                elif kind == "renderer.snapshot":
                    _worker_result(channel, request, {"rows": session.snapshot()})
                elif kind == "renderer.export":
                    _worker_result(channel, request, {"rows": session.export()})
                elif kind == "renderer.click":
                    _worker_result(channel, request, {
                        "result": session.dispatch_click(int(request["payload"]["node_idx"]))})
                elif kind == "renderer.event":
                    _worker_result(channel, request, {
                        "result": session.dispatch_event(*request["payload"]["args"])})
                elif kind == "renderer.set_attr":
                    p = request["payload"]
                    _worker_result(channel, request, {"result": session.set_attr(
                        int(p["node_idx"]), p["name"], p["value"])})
                elif kind == "renderer.remove_attr":
                    p = request["payload"]
                    _worker_result(channel, request, {"result": session.remove_attr(
                        int(p["node_idx"]), p["name"])})
                elif kind == "renderer.state":
                    _worker_result(channel, request, asdict(session.state()))
                elif kind == "renderer.has_pending":
                    _worker_result(channel, request, {
                        "pending": session.has_pending_work()})
                elif kind == "renderer.set_focus":
                    value = request["payload"].get("node_idx")
                    session.doc.set_focus(value)
                    _worker_result(channel, request, {"result": None})
                elif kind == "renderer.set_hover":
                    session.doc.set_hover(request["payload"].get("node_idx"))
                    _worker_result(channel, request, {"result": None})
                elif kind == "renderer.set_layout_rects":
                    # JSON decodes every row as a list; the native
                    # binding only extracts real tuples
                    session.doc.set_layout_rects(
                        [tuple(r)
                         for r in request["payload"].get("rects", [])])
                    _worker_result(channel, request, {"result": None})
                elif kind == "renderer.set_scroll_state":
                    session.set_scroll_state(
                        [tuple(r)
                         for r in request["payload"].get("state", [])])
                    _worker_result(channel, request, {"result": None})
                elif kind == "renderer.take_scroll_writes":
                    _worker_result(channel, request, {
                        "writes": session.take_scroll_writes()})
                elif kind == "renderer.take_scroll_into_view":
                    _worker_result(channel, request, {
                        "nodes": session.take_scroll_into_view()})
                elif kind == "renderer.set_frame_graph":
                    session.set_frame_graph(
                        [tuple(r)
                         for r in request["payload"].get("frames", [])])
                    _worker_result(channel, request, {"result": None})
                elif kind == "renderer.take_frame_writes":
                    _worker_result(channel, request, {
                        "messages": session.take_frame_writes()})
                elif kind == "renderer.deliver_message":
                    p = request["payload"]
                    _worker_result(channel, request, {
                        "logs": session.deliver_message(
                            p.get("data", "null"), p.get("origin", "null"),
                            int(p.get("source", 0)))})
                elif kind == "renderer.set_frame_document":
                    p = request["payload"]
                    rows = json.loads(read_blob(p["rows_blob"]))
                    # JSON flattens every tuple, including the nested
                    # attribute pairs, and the binding wants real ones
                    session.set_frame_document(
                        int(p.get("node", 0)), int(p.get("handle", 0)),
                        p.get("url", ""),
                        [(a, b, c, d, [tuple(x) for x in e])
                         for a, b, c, d, e in rows])
                    _worker_result(channel, request, {"result": None})
                elif kind == "renderer.take_frame_dom_writes":
                    _worker_result(channel, request, {
                        "writes": session.take_frame_dom_writes()})
                elif kind == "renderer.set_text_content":
                    p = request["payload"]
                    session.set_text_content(
                        int(p.get("node_idx", 0)), p.get("text", ""))
                    _worker_result(channel, request, {"result": None})
                elif kind == "renderer.restyle_diff":
                    outcome, patches = session.doc.restyle_diff(session.css_sources)
                    _worker_result(channel, request, {
                        "outcome": outcome, "patches": patches})
                elif kind == "renderer.close":
                    if session is not None:
                        session.close()
                        session = None
                    _worker_result(channel, request, {"closed": True})
                elif kind == "renderer.test_stale":
                    channel.send(
                        "renderer.response", payload={"stale": True},
                        reply_to=request["msg_id"],
                        generation=max(0, generation - 1),
                        document_token=request["document_token"])
                    _worker_result(channel, request, {"stale": False})
                elif kind == "renderer.test_hang":
                    time.sleep(float(request["payload"].get("seconds", 60)))
                    _worker_result(channel, request, {"hung": False})
                elif kind == "renderer.test_crash":
                    os._exit(91)
                elif kind == "renderer.shutdown":
                    if session is not None:
                        session.close()
                    _worker_result(channel, request, {"shutdown": True})
                    break
                else:
                    raise ProtocolError("unknown_type", f"unsupported request {kind}")
            except Exception as exc:
                _worker_error(channel, request, exc)
    except (EOFError, BrokenPipeError):
        pass
    except ProtocolError as exc:
        try:
            channel.send("protocol_error", payload={
                "code": exc.code, "message": str(exc)[:4096]})
        except Exception:
            pass
    finally:
        if session is not None:
            try:
                session.close()
            except Exception:
                pass
        channel.close()


class RendererProcessHost:
    """Own one renderer process and broker its network capabilities."""

    def __init__(self, network_backend, *, startup_timeout=10.0,
                 request_timeout=15.0):
        self.network = network_backend
        self.startup_timeout = float(startup_timeout)
        self.request_timeout = float(request_timeout)
        self.renderer_id = "r-" + uuid.uuid4().hex
        self.document_token = ""
        self.generation = 0
        self.process = None
        self.channel = None
        self.dead_reason = None
        self._contexts = {}
        self._active_cancel_token = None
        self._in_flight = 0
        self.sandbox_report = []
        self.start()

    @property
    def alive(self):
        return bool(self.process and self.process.is_alive() and self.channel)

    @property
    def pid(self):
        return self.process.pid if self.process is not None else None

    def start(self):
        self._cleanup_process()
        context = multiprocessing.get_context("spawn")
        parent, child = context.Pipe(duplex=True)
        process = context.Process(
            target=renderer_worker, args=(child, self.renderer_id),
            name=f"gg-renderer-{self.renderer_id[-8:]}", daemon=True)
        process.start()
        child.close()
        self.process = process
        self.channel = JsonChannel(parent, self.renderer_id)
        self.dead_reason = None
        nonce = secrets.token_hex(32)
        hello = self.channel.send("hello", payload={
            "role": "browser", "build_id": BUILD_ID, "nonce": nonce})
        if not self.channel.poll(self.startup_timeout):
            self._mark_dead("handshake timeout", terminate=True)
            raise RendererHung("renderer handshake timed out")
        response = self.channel.recv()
        payload = response["payload"]
        if response["type"] != "hello_ack" \
                or response["reply_to"] != hello["msg_id"] \
                or payload.get("role") != "renderer" \
                or payload.get("build_id") != BUILD_ID \
                or payload.get("nonce") != nonce:
            self._mark_dead("invalid handshake", terminate=True)
            raise ProtocolError("bad_handshake", "renderer handshake failed")
        self.sandbox_report = list(payload.get("sandbox") or [])
        return self

    def restart(self):
        self.shutdown(force=True)
        self.renderer_id = "r-" + uuid.uuid4().hex
        self.document_token = ""
        self.generation = 0
        return self.start()

    def call(self, message_type, payload=None, *, timeout=None,
             advance_generation=False, document_token=None,
             cancel_token=None):
        if not self.alive:
            if message_type == "renderer.commit":
                self.restart()
            else:
                raise RendererCrashed(self.dead_reason or "renderer is not alive")
        if self._in_flight >= MAX_IN_FLIGHT:
            raise RendererProcessError("renderer in-flight quota exceeded")
        if advance_generation:
            self.generation += 1
            self.document_token = document_token or secrets.token_hex(16)
        token = self.document_token if document_token is None else document_token
        limit = self.request_timeout if timeout is None else float(timeout)
        body = dict(payload or {})
        body["deadline_ms"] = int((time.monotonic() + limit) * 1000)
        leases = BlobStore()
        self._active_cancel_token = cancel_token
        self._in_flight += 1
        try:
            request = self.channel.send(
                message_type, payload=body, generation=self.generation,
                document_token=token)
            deadline = time.monotonic() + limit
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    self._mark_dead(f"{message_type} timed out", terminate=True)
                    raise RendererHung(f"renderer request timed out: {message_type}")
                if not self.channel.poll(min(remaining, 0.05)):
                    if not self.process.is_alive():
                        self._mark_dead(
                            f"renderer exited with code {self.process.exitcode}")
                        raise RendererCrashed(self.dead_reason)
                    continue
                try:
                    message = self.channel.recv()
                except (EOFError, BrokenPipeError, OSError) as exc:
                    self._mark_dead("renderer IPC closed")
                    raise RendererCrashed(self.dead_reason) from exc
                if message["generation"] < self.generation:
                    continue
                if message["type"].startswith("broker.") \
                        and message["reply_to"] is None:
                    self._handle_broker(message, leases)
                    continue
                if message["reply_to"] != request["msg_id"]:
                    self._mark_dead("unexpected renderer response", terminate=True)
                    raise ProtocolError(
                        "unexpected_reply",
                        "renderer response did not match request: "
                        f"type={message['type']} reply_to={message['reply_to']} "
                        f"expected={request['msg_id']}")
                if message["type"] == "renderer.error":
                    error = message["payload"]
                    raise RemoteRendererError(
                        f"{error.get('name', 'RendererError')}: "
                        f"{error.get('message', '')}")
                if message["type"] == "protocol_error":
                    error = message["payload"]
                    raise ProtocolError(error.get("code", "peer_error"),
                                        error.get("message", "protocol error"))
                if message["type"] != "renderer.response":
                    raise ProtocolError("unexpected_type", "expected renderer.response")
                return message["payload"]
        finally:
            self._in_flight -= 1
            self._active_cancel_token = None
            leases.release_all()

    def _network_kwargs(self, payload):
        kwargs = dict(payload.get("kwargs") or {})
        context_id = kwargs.pop("context_id", None)
        if context_id is not None:
            context = self._contexts.get(context_id)
            if context is None:
                raise ProtocolError("invalid_context", "unknown network context")
            kwargs["context"] = context
        if self._active_cancel_token is not None:
            kwargs["cancel_token"] = self._active_cancel_token
        return kwargs

    def _handle_broker(self, message, leases):
        kind = message["type"]
        p = message["payload"]
        try:
            deadline_ms = p.get("deadline_ms")
            if deadline_ms is None or time.monotonic() * 1000 > deadline_ms:
                raise TimeoutError("broker request deadline expired")
            if kind == "broker.bind_context":
                context = self.network.bind_context(
                    net.URL(p["document_url"]), profile_id=p.get("profile_id", "default"))
                context_id = "ctx-" + uuid.uuid4().hex
                self._contexts[context_id] = context
                result = {"context": {"context_id": context_id}}
            elif kind == "broker.drop_context":
                context = self._contexts.pop(p.get("context_id"), None)
                result = {"dropped": bool(
                    context is not None and self.network.drop_context(context))}
            elif kind in {"broker.request_text", "broker.request", "broker.request_raw"}:
                url = net.URL(p["url"])
                kwargs = self._network_kwargs(p)
                if kind == "broker.request_text":
                    headers, body, final_url = self.network.request_text(url, **kwargs)
                    result = {"headers": headers, "final_url": str(final_url),
                              "body_blob": leases.pack(body.encode("utf-8"), "text/plain")}
                elif kind == "broker.request":
                    headers, body = self.network.request(url, **kwargs)
                    result = {"headers": headers,
                              "body_blob": leases.pack(body.encode("utf-8"), "text/plain")}
                else:
                    headers, body = self.network.request_raw(url, **kwargs)
                    result = {"headers": headers,
                              "body_blob": leases.pack(body)}
            elif kind == "broker.script_fetch":
                response = self.network.perform_script_fetch(
                    net.URL(p["base_url"]), p["request"],
                    **self._network_kwargs(p))
                result = {
                    "status": response.status, "headers": response.headers,
                    "final_url": str(response.final_url), "opaque": response.opaque,
                    "body_blob": leases.pack(response.body.encode("utf-8"), "text/plain"),
                }
            elif kind == "broker.cookies_for":
                result = {"value": self.network.cookies_for(
                    net.URL(p["url"]), **self._network_kwargs(p))}
            elif kind == "broker.cookie_set":
                result = {"accepted": bool(self.network.set_cookie_from_js(
                    net.URL(p["url"]), p["value"], **self._network_kwargs(p)))}
            else:
                raise ProtocolError("unknown_type", f"unsupported broker request {kind}")
            self.channel.send(
                "broker.response", payload=result,
                reply_to=message["msg_id"], generation=message["generation"],
                document_token=message["document_token"])
        except Exception as exc:
            self.channel.send(
                "broker.error",
                payload={"name": type(exc).__name__, "message": str(exc)[:4096]},
                reply_to=message["msg_id"], generation=message["generation"],
                document_token=message["document_token"])

    def _mark_dead(self, reason, terminate=False):
        self.dead_reason = str(reason)
        if terminate and self.process is not None and self.process.is_alive():
            self.process.terminate()
            self.process.join(timeout=1.0)
        if self.channel is not None:
            self.channel.close()
            self.channel = None
        self._drop_contexts()

    def _drop_contexts(self):
        for context in self._contexts.values():
            try:
                self.network.drop_context(context)
            except Exception:
                pass
        self._contexts.clear()

    def _cleanup_process(self):
        if self.channel is not None:
            self.channel.close()
            self.channel = None
        if self.process is not None:
            if self.process.is_alive():
                self.process.terminate()
            self.process.join(timeout=1.0)
            self.process = None
        self._drop_contexts()

    def shutdown(self, force=False):
        if self.alive and not force:
            try:
                self.call("renderer.shutdown", timeout=2.0)
            except RendererProcessError:
                force = True
        if force and self.process is not None and self.process.is_alive():
            self.process.terminate()
        self._cleanup_process()

    def test_crash(self):
        return self.call("renderer.test_crash", timeout=2.0)

    def test_hang(self, timeout=0.1):
        return self.call(
            "renderer.test_hang", {"seconds": timeout * 10}, timeout=timeout)

    def test_stale(self):
        return self.call("renderer.test_stale", timeout=2.0)
