"""Network service child process and the browser-side client.

The cookie jar, the connection pool and the HTTP cache are the profile's
authority: they hold credentials for every site the user has visited.
Keeping them in the browser process means a browser-process bug reaches
all of them at once, so they move here, behind the same validated JSON
channel the renderer already speaks.

Two things shape the transport, and both are about not becoming the
bottleneck the design document warns against:

* **Request-id routing.** Page loads fan a dozen subresource fetches
  across worker threads. A lock-step request/response pipe would
  serialize them all, so the browser side runs one reader thread that
  matches replies by `reply_to` and hands each waiting caller its own.
* **A reader loop that never blocks on I/O.** The service dispatches
  every fetch to a thread pool, so its reader loop stays free to accept
  the next request — and, more importantly, a *cancel*, which is
  useless if it queues behind the request it is trying to stop.
"""

from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
import multiprocessing
import os
import secrets
import threading
import time
import uuid

from .. import net
from ..ipc.blobs import BlobStore, read_blob
from ..ipc.channel import JsonChannel
from ..ipc.protocol import BUILD_ID, ProtocolError
from ..ipc.wire import from_wire, to_wire
from ..network_backend import NetworkContext

# One page load is a handful of parallel subresource fetches; more
# threads than that only deepens the queue behind a slow origin.
SERVICE_WORKERS = 8
DEFAULT_TIMEOUT = 20.0


class NetworkProcessError(RuntimeError):
    pass


class NetworkCrashed(NetworkProcessError):
    pass


class NetworkHung(NetworkProcessError):
    pass


class RemoteNetworkError(NetworkProcessError):
    """The service refused or failed a request in a way the caller has
    no specific handling for."""

    def __init__(self, name, message):
        super().__init__(f"{name}: {message}")
        self.name = str(name)
        self.detail = str(message)


# Callers branch on these by type — `except net.RequestCancelled` is how
# a cancelled navigation is told apart from a failed one — so the class
# has to survive the hop, not just the message.
_TYPED_ERRORS = {
    "RequestCancelled": net.RequestCancelled,
    "TimeoutError": TimeoutError,
    "ValueError": ValueError,
}


def _raise_remote(name, message):
    cls = _TYPED_ERRORS.get(name)
    raise cls(message) if cls is not None else RemoteNetworkError(
        name, message)


# ---------------------------------------------------------------------
# service side
# ---------------------------------------------------------------------


def _reply(channel, request, payload):
    channel.send("network.response", payload=payload,
                 reply_to=request["msg_id"])


def _reply_error(channel, request, exc):
    channel.send(
        "network.error",
        payload={"name": type(exc).__name__, "message": str(exc)[:4096]},
        reply_to=request["msg_id"])


def _serve(channel, request, state):
    kind = request["type"]
    payload = request["payload"]
    blobs = BlobStore()
    try:
        deadline_ms = payload.get("deadline_ms")
        if deadline_ms is not None and time.monotonic() * 1000 > deadline_ms:
            raise TimeoutError("network request deadline expired")

        if kind == "network.bind_context":
            context = NetworkContext(
                uuid.uuid4().hex, str(payload.get("profile_id", "default")),
                str(payload.get("document_url", "")))
            state["contexts"][context.context_id] = context
            _reply(channel, request, {"context": {
                "context_id": context.context_id,
                "profile_id": context.profile_id,
                "document_url": context.document_url,
            }})
            return
        if kind == "network.drop_context":
            dropped = state["contexts"].pop(
                payload.get("context_id"), None) is not None
            _reply(channel, request, {"dropped": dropped})
            return

        kwargs = from_wire(
            payload.get("kwargs"), contexts=state["contexts"],
            cancel_token=_bind_token(state, payload.get("cancel_id")))
        kwargs.pop("context", None)   # the service *is* the profile
        url = net.URL(payload["url"]) if payload.get("url") else None

        if kind == "network.request_text":
            headers, body, final = net.request_text(url, **kwargs)
            _reply(channel, request, {
                "headers": headers, "final_url": str(final),
                "body_blob": blobs.pack(body.encode("utf-8"), "text/html")})
        elif kind == "network.request":
            headers, body = net.request(url, **kwargs)
            _reply(channel, request, {
                "headers": headers,
                "body_blob": blobs.pack(body.encode("utf-8"), "text/plain")})
        elif kind == "network.request_raw":
            headers, body = net.request_raw(url, **kwargs)
            _reply(channel, request, {
                "headers": headers,
                "body_blob": blobs.pack(body, "application/octet-stream")})
        elif kind == "network.script_fetch":
            from ..security import perform_script_fetch

            response = perform_script_fetch(
                net.URL(payload["base_url"]), payload["request"], **kwargs)
            _reply(channel, request, {
                "status": response.status, "headers": response.headers,
                "final_url": str(response.final_url),
                "opaque": bool(response.opaque),
                "body_blob": blobs.pack(
                    response.body.encode("utf-8"), "text/javascript")})
        elif kind == "network.cookies_for":
            _reply(channel, request, {"value": net.cookies_for(url)})
        elif kind == "network.cookie_set":
            _reply(channel, request, {"accepted": net.set_cookie_from_js(
                url, str(payload.get("value", "")))})
        else:
            raise ProtocolError("unknown_type", f"unknown type {kind!r}")
    except Exception as exc:      # a failed fetch is data, not a crash
        try:
            _reply_error(channel, request, exc)
        except Exception:
            pass
    finally:
        state["tokens"].pop(request["payload"].get("cancel_id"), None)
        blobs.release_all()


def _bind_token(state, cancel_id):
    """A live token the service can cancel from its reader loop."""
    if cancel_id is None:
        return None
    token = state["tokens"].get(cancel_id)
    if token is None:
        token = net.CancellationToken()
        state["tokens"][cancel_id] = token
    return token


def network_worker(connection, service_id):
    """Spawn target. Owns the profile's cookies, cache and sockets."""
    channel = JsonChannel(connection, service_id)
    state = {"contexts": {}, "tokens": {}}
    pool = ThreadPoolExecutor(max_workers=SERVICE_WORKERS)
    try:
        hello = channel.recv()
        if hello["type"] != "hello":
            raise ProtocolError(
                "handshake_required", "first message must be hello")
        hp = hello["payload"]
        if hp.get("role") != "browser" or hp.get("build_id") != BUILD_ID \
                or not isinstance(hp.get("nonce"), str):
            raise ProtocolError("bad_handshake", "handshake identity mismatch")
        channel.send(
            "hello_ack", reply_to=hello["msg_id"],
            payload={"role": "network", "build_id": BUILD_ID,
                     "nonce": hp["nonce"], "pid": os.getpid()})

        while True:
            try:
                request = channel.recv()
            except (EOFError, OSError):
                break
            kind = request["type"]
            if kind == "network.shutdown":
                break
            if kind == "network.cancel":
                # handled inline, never queued: a cancel that waits
                # behind the request it is stopping is not a cancel
                token = state["tokens"].get(
                    request["payload"].get("cancel_id"))
                if token is not None:
                    token.cancel()
                _reply(channel, request, {"cancelled": token is not None})
                continue
            if kind == "network.cancel_new":
                cancel_id = request["payload"].get("cancel_id")
                _bind_token(state, cancel_id)
                _reply(channel, request, {"cancel_id": cancel_id})
                continue
            pool.submit(_serve, channel, request, state)
    except Exception:
        pass
    finally:
        pool.shutdown(wait=False)
        channel.close()


# ---------------------------------------------------------------------
# browser side
# ---------------------------------------------------------------------


class _Waiter:
    __slots__ = ("event", "message")

    def __init__(self):
        self.event = threading.Event()
        self.message = None


class NetworkProcessHost:
    """Own one network service process and route replies to callers."""

    def __init__(self, *, startup_timeout=10.0,
                 request_timeout=DEFAULT_TIMEOUT):
        self.service_id = "n-" + uuid.uuid4().hex
        self.startup_timeout = float(startup_timeout)
        self.request_timeout = float(request_timeout)
        self.process = None
        self.channel = None
        self.dead_reason = None
        self._waiters = {}
        self._lock = threading.Lock()
        self._reader = None
        self.start()

    @property
    def alive(self):
        return bool(self.process and self.process.is_alive() and self.channel)

    @property
    def pid(self):
        return self.process.pid if self.process is not None else None

    def start(self):
        self._cleanup()
        context = multiprocessing.get_context("spawn")
        parent, child = context.Pipe(duplex=True)
        process = context.Process(
            target=network_worker, args=(child, self.service_id),
            name=f"gg-network-{self.service_id[-8:]}", daemon=True)
        process.start()
        child.close()
        self.process = process
        self.channel = JsonChannel(parent, self.service_id)
        self.dead_reason = None
        nonce = secrets.token_hex(32)
        hello = self.channel.send("hello", payload={
            "role": "browser", "build_id": BUILD_ID, "nonce": nonce})
        if not self.channel.poll(self.startup_timeout):
            self._mark_dead("handshake timeout")
            raise NetworkHung("network service handshake timed out")
        response = self.channel.recv()
        payload = response["payload"]
        if response["type"] != "hello_ack" \
                or response["reply_to"] != hello["msg_id"] \
                or payload.get("role") != "network" \
                or payload.get("build_id") != BUILD_ID \
                or payload.get("nonce") != nonce:
            self._mark_dead("invalid handshake")
            raise ProtocolError("bad_handshake", "network handshake failed")
        # only after the handshake, so it cannot race the ack
        self._reader = threading.Thread(
            target=self._read_loop, name=f"{self.service_id}-reader",
            daemon=True)
        self._reader.start()
        return self

    def _read_loop(self):
        while True:
            channel = self.channel
            if channel is None:
                return
            try:
                message = channel.recv()
            except Exception as exc:
                self._fail_all(exc)
                return
            waiter = None
            with self._lock:
                waiter = self._waiters.pop(message.get("reply_to"), None)
            if waiter is not None:
                waiter.message = message
                waiter.event.set()

    def _fail_all(self, exc):
        with self._lock:
            waiters = list(self._waiters.values())
            self._waiters.clear()
        self.dead_reason = self.dead_reason or str(exc)
        for waiter in waiters:
            waiter.message = None
            waiter.event.set()

    def call(self, message_type, payload=None, *, timeout=None):
        if not self.alive:
            raise NetworkCrashed(self.dead_reason or "network service is down")
        limit = self.request_timeout if timeout is None else float(timeout)
        body = dict(payload or {})
        body["deadline_ms"] = int((time.monotonic() + limit) * 1000)
        waiter = _Waiter()
        msg_id = self.channel.next_id()
        with self._lock:
            self._waiters[msg_id] = waiter
        try:
            self.channel.send(message_type, payload=body, msg_id=msg_id)
        except Exception:
            with self._lock:
                self._waiters.pop(msg_id, None)
            raise
        if not waiter.event.wait(limit + 1.0):
            with self._lock:
                self._waiters.pop(msg_id, None)
            raise NetworkHung(f"network request timed out: {message_type}")
        message = waiter.message
        if message is None:
            raise NetworkCrashed(
                self.dead_reason or "network service went away")
        if message["type"] == "network.error":
            error = message["payload"]
            _raise_remote(
                error.get("name", "NetworkError"), error.get("message", ""))
        if message["type"] != "network.response":
            raise ProtocolError(
                "unexpected_type", "expected network.response")
        return message["payload"]

    def post(self, message_type, payload=None):
        """Fire and forget — used for cancel, which must not wait on the
        request it is cancelling."""
        if not self.alive:
            return False
        try:
            self.channel.send(message_type, payload=dict(payload or {}))
            return True
        except Exception:
            return False

    def _mark_dead(self, reason):
        self.dead_reason = reason
        self._fail_all(NetworkCrashed(reason))
        self._cleanup()

    def _cleanup(self):
        if self.channel is not None:
            self.channel.close()
            self.channel = None
        if self.process is not None and self.process.is_alive():
            self.process.terminate()
            self.process.join(timeout=2.0)
        self.process = None

    def shutdown(self):
        if self.alive:
            self.post("network.shutdown")
            self.process.join(timeout=2.0)
        self._cleanup()


class _RemoteCancelToken:
    """A token whose authority lives in the service.

    `cancel()` posts rather than calls: the request it is stopping is
    still in flight, so waiting for a reply on the same channel would
    wait for the very thing being cancelled.
    """

    __slots__ = ("host", "cancel_id", "cancelled")

    def __init__(self, host):
        self.host = host
        self.cancel_id = uuid.uuid4().hex
        self.cancelled = False

    def cancel(self):
        first = not self.cancelled
        self.cancelled = True
        if first:
            self.host.post("network.cancel", {"cancel_id": self.cancel_id})
        return first

    def check(self):
        if self.cancelled:
            raise net.RequestCancelled("request cancelled")


class RemoteNetworkBackend:
    """NetworkBackend implemented against the network service."""

    def __init__(self, host=None, *, timeout=DEFAULT_TIMEOUT):
        self.host = host or NetworkProcessHost(request_timeout=timeout)
        self.timeout = float(timeout)

    # -- capability lifetime -------------------------------------------

    def bind_context(self, document_url, *, profile_id="default"):
        return self.host.call("network.bind_context", {
            "document_url": str(document_url),
            "profile_id": str(profile_id)}, timeout=self.timeout)["context"]

    def drop_context(self, context):
        return self.host.call("network.drop_context", {
            "context_id": (context or {}).get("context_id"),
        }, timeout=self.timeout)["dropped"]

    def new_cancel_token(self):
        return _RemoteCancelToken(self.host)

    def cancel(self, token):
        return token.cancel() if token is not None else False

    # -- fetching -------------------------------------------------------

    def _payload(self, url, kwargs, **extra):
        token = kwargs.get("cancel_token")
        body = {"url": str(url), "kwargs": to_wire(kwargs)}
        if isinstance(token, _RemoteCancelToken):
            body["cancel_id"] = token.cancel_id
        body.update(extra)
        return body

    def _timeout(self, kwargs):
        # the service needs longer than the caller, or a hung socket
        # surfaces as a service timeout instead of the real one
        return float(kwargs.get("timeout") or self.timeout) + 5.0

    def request_text(self, url, **kwargs):
        payload = self.host.call(
            "network.request_text", self._payload(url, kwargs),
            timeout=self._timeout(kwargs))
        return (payload["headers"],
                read_blob(payload["body_blob"]).decode(
                    "utf-8", errors="replace"),
                net.URL(payload["final_url"]))

    def request(self, url, **kwargs):
        payload = self.host.call(
            "network.request", self._payload(url, kwargs),
            timeout=self._timeout(kwargs))
        return (payload["headers"],
                read_blob(payload["body_blob"]).decode(
                    "utf-8", errors="replace"))

    def request_raw(self, url, **kwargs):
        payload = self.host.call(
            "network.request_raw", self._payload(url, kwargs),
            timeout=self._timeout(kwargs))
        return payload["headers"], read_blob(payload["body_blob"])

    def perform_script_fetch(self, base_url, request, **kwargs):
        from ..security import ScriptFetchResponse

        payload = self.host.call(
            "network.script_fetch",
            self._payload("", kwargs, base_url=str(base_url),
                          request=request),
            timeout=self._timeout(kwargs))
        return ScriptFetchResponse(
            payload["status"], payload["headers"],
            read_blob(payload["body_blob"]).decode("utf-8", errors="replace"),
            net.URL(payload["final_url"]), payload.get("opaque", False))

    # -- cookies --------------------------------------------------------

    def cookies_for(self, url, **kwargs):
        return self.host.call(
            "network.cookies_for", self._payload(url, kwargs),
            timeout=self.timeout)["value"]

    def set_cookie_from_js(self, url, value, **kwargs):
        return self.host.call(
            "network.cookie_set",
            self._payload(url, kwargs, value=str(value)),
            timeout=self.timeout)["accepted"]

    def close(self):
        self.host.shutdown()
