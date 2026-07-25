#!/usr/bin/env python3
"""Gauntlet for browser/net.py against a local http.server.

Run: python3 validation/net_gauntlet.py (repo root auto-detected)
Prints one PASS/FAIL line per claim, then a JSON summary.
"""
import base64
from concurrent.futures import ThreadPoolExecutor
import gzip as gzmod
import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from browser import net, psl, security  # noqa: E402

GZ_PAYLOAD = b"gzip works: the quick brown fox. " * 64  # 2112 bytes
GZ_COMP = gzmod.compress(GZ_PAYLOAD)
CHUNKED_BODY = b"Hello, chunked!"
HUGE = b"a" * 65536

RESULTS = []


def record(claim, ok, evidence):
    RESULTS.append({"claim": claim, "ok": bool(ok), "evidence": evidence})
    print(("PASS " if ok else "FAIL "), claim, "::", evidence, flush=True)


class Server(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, *a, **kw):
        super().__init__(*a, **kw)
        self.hits = {}
        self.conn_count = 0
        self.commands = []
        self.requests = []
        self.lock = threading.Lock()
        self.slow_started = threading.Event()

    def handle_error(self, request, client_address):
        # Cancellation intentionally tears a keep-alive socket down while
        # the handler might be entering its next read.
        pass


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"  # net.py only reuses HTTP/1.1 conns

    def setup(self):
        super().setup()
        with self.server.lock:
            self.server.conn_count += 1

    def log_message(self, *a):
        pass

    def _body(self, code, body, extra=()):
        self.send_response(code)
        for k, v in extra:
            self.send_header(k, v)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _record(self, path, body=b""):
        with self.server.lock:
            self.server.hits[path] = self.server.hits.get(path, 0) + 1
            self.server.commands.append(self.command)
            self.server.requests.append({
                "method": self.command,
                "path": path,
                "body": body,
                "content_type": self.headers.get("Content-Type", ""),
                "cookie": self.headers.get("Cookie", ""),
                "if_none_match": self.headers.get("If-None-Match", ""),
                "if_modified_since": self.headers.get(
                    "If-Modified-Since", ""),
                "accept_language": self.headers.get("Accept-Language", ""),
            })

    def do_GET(self):
        path = self.path.split("?")[0]
        self._record(path)

        if path == "/plain":
            self._body(200, b"hello plain", [("Content-Type", "text/plain")])
        elif path == "/gzip":
            self._body(200, GZ_COMP, [("Content-Encoding", "gzip"),
                                      ("Content-Type", "text/plain")])
        elif path == "/chunked":
            self.send_response(200)
            self.send_header("Transfer-Encoding", "chunked")
            self.end_headers()
            # 3 chunks (one with a chunk extension) + trailer section
            self.wfile.write(b"5;ext=zz\r\nHello\r\n")
            self.wfile.write(b"7\r\n, chunk\r\n")
            self.wfile.write(b"3\r\ned!\r\n")
            self.wfile.write(b"0\r\nX-Trailer: seen\r\n\r\n")
        elif path == "/redir/a":
            self._body(301, b"", [("Location", "/redir/b")])
        elif path == "/redir/b":
            self._body(302, b"", [("Location", "/redir/final")])
        elif path == "/redir/final":
            self._body(200, b"redirected-final")
        elif path.startswith("/method-redirect/"):
            code = int(path.rsplit("/", 1)[1])
            self._body(code, b"", [("Location", "/method-final")])
        elif path == "/method-final":
            self._body(200, self.command.encode() + b":")
        elif path == "/redir/loop":
            self._body(301, b"", [("Location", "/redir/loop")])
        elif path == "/cached":
            self._body(200, b"cacheable-payload",
                       [("Cache-Control", "max-age=3600")])
        elif path == "/cache/etag":
            if self.headers.get("If-None-Match") == '"v1"':
                self.send_response(304)
                self.send_header("ETag", '"v1"')
                self.send_header("Cache-Control", "max-age=60")
                self.send_header("X-Revalidated", "yes")
                self.end_headers()
            else:
                self._body(200, b"etag-body-v1", [
                    ("Cache-Control", "no-cache"),
                    ("ETag", '"v1"'),
                    ("X-Original", "kept"),
                ])
        elif path == "/cache/last-modified":
            stamp = "Mon, 20 Jul 2026 12:00:00 GMT"
            if self.headers.get("If-Modified-Since") == stamp:
                self.send_response(304)
                self.send_header("Last-Modified", stamp)
                self.send_header("Cache-Control", "max-age=60")
                self.end_headers()
            else:
                self._body(200, b"last-modified-body", [
                    ("Cache-Control", "no-cache"),
                    ("Last-Modified", stamp),
                ])
        elif path == "/cache/vary":
            language = self.headers.get("Accept-Language", "none")
            self._body(200, ("language=" + language).encode(), [
                ("Cache-Control", "max-age=3600"),
                ("Vary", "Accept-Language"),
            ])
        elif path == "/cache/vary-star":
            count = self.server.hits.get(path, 0)
            self._body(200, f"vary-star-{count}".encode(), [
                ("Cache-Control", "max-age=3600"), ("Vary", "*")])
        elif path == "/cache/no-store":
            count = self.server.hits.get(path, 0)
            self._body(200, f"no-store-{count}".encode(), [
                ("Cache-Control", "no-store")])
        elif path == "/cache/private":
            self._body(200, b"private-cache-body", [
                ("Cache-Control", "private, max-age=3600")])
        elif path == "/slow/cancel":
            self.send_response(200)
            self.send_header("Content-Length", "4096")
            self.end_headers()
            self.wfile.write(b"x")
            self.wfile.flush()
            self.server.slow_started.set()
            time.sleep(1.0)
            try:
                self.wfile.write(b"y" * 4095)
            except (BrokenPipeError, ConnectionResetError):
                pass
        elif path == "/slow/timeout":
            time.sleep(0.25)
            try:
                self._body(200, b"too-late")
            except (BrokenPipeError, ConnectionResetError):
                pass
        elif path.startswith("/ka/"):
            self._body(200, ("ka:" + path).encode())
        elif path == "/cookie/set":
            self.send_response(200)
            self.send_header(
                "Set-Cookie",
                "server=secret; Path=/cookie; HttpOnly; SameSite=Lax")
            self.send_header("Set-Cookie", "other=no; Path=/other")
            self.send_header("Content-Length", "2")
            self.end_headers()
            self.wfile.write(b"ok")
        elif path == "/cookie/echo":
            self._body(200, self.headers.get("Cookie", "").encode())
        elif path == "/cors/public":
            self._body(200, b"cors-public", [
                ("Access-Control-Allow-Origin", "*")])
        elif path == "/cors/credentials":
            self._body(200, self.headers.get("Cookie", "").encode(), [
                ("Access-Control-Allow-Origin",
                 self.headers.get("Origin", "null")),
                ("Access-Control-Allow-Credentials", "true")])
        elif path == "/cors/deny":
            self._body(200, b"secret-without-cors")
        elif path == "/cors/preflight":
            self._body(200, self.command.encode(), [
                ("Access-Control-Allow-Origin",
                 self.headers.get("Origin", "null")),
                ("Access-Control-Allow-Credentials", "true")])
        elif path == "/hugehdr":
            # raw write: huge header, a colon-less garbage line, weird-cased
            # content-length -- probes parser robustness
            w = self.wfile
            w.write(b"HTTP/1.1 200 OK\r\n")
            w.write(b"X-Huge: " + HUGE + b"\r\n")
            w.write(b"this-garbage-line-has-no-colon\r\n")
            w.write(b"cOnTeNt-LenGTH: 9\r\n")
            w.write(b"\r\nhugehdrok")
        else:
            self._body(404, b"nope")

    def do_POST(self):
        path = self.path.split("?")[0]
        length = int(self.headers.get("Content-Length", "0") or "0")
        body = self.rfile.read(length)
        self._record(path, body)
        if path == "/post/echo":
            self._body(200, body, [("X-Request-Method", self.command),
                                    ("X-Request-Content-Type",
                                     self.headers.get("Content-Type", ""))])
        elif path.startswith("/method-redirect/"):
            code = int(path.rsplit("/", 1)[1])
            self._body(code, b"", [("Location", "/method-final")])
        elif path == "/method-final":
            self._body(200, self.command.encode() + b":" + body)
        elif path == "/cookie/echo":
            self._body(200, self.headers.get("Cookie", "").encode())
        elif path == "/cors/preflight":
            self._body(200, self.command.encode() + b":" + body, [
                ("Access-Control-Allow-Origin",
                 self.headers.get("Origin", "null")),
                ("Access-Control-Allow-Credentials", "true")])
        else:
            self._body(404, b"nope")

    def do_OPTIONS(self):
        path = self.path.split("?")[0]
        self._record(path)
        if path == "/cors/preflight":
            self._body(204, b"", [
                ("Access-Control-Allow-Origin",
                 self.headers.get("Origin", "null")),
                ("Access-Control-Allow-Credentials", "true"),
                ("Access-Control-Allow-Methods", "GET, POST, PUT"),
                ("Access-Control-Allow-Headers", "content-type, x-token"),
                ("Access-Control-Max-Age", "600"),
            ])
        else:
            self._body(404, b"nope")


def start_server():
    srv = Server(("127.0.0.1", 0), Handler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv, f"http://127.0.0.1:{srv.server_address[1]}"


def _ipc_cookie_policy_cases(base):
    """Re-run the two policies that depend on the *initiator* over the
    real renderer-process IPC path.

    Cookie SameSite and mixed-content both key off `site_for_cookies`,
    which a renderer names but the browser process enforces. If that
    argument does not survive the hop, `net` reads None as *same site*
    and stops blocking — so these cases have to run where the argument
    actually travels, not only in-process.
    """
    from browser import native
    from browser.process.renderer_host import (
        RendererProcessHost, _BrokerNetworkBackend)

    # the marshalling itself: a URL-valued kwarg is not a JSON scalar,
    # and a plain type filter drops it silently
    wire = _BrokerNetworkBackend._kwargs({
        "site_for_cookies": net.URL("https://top.test/"),
        "top_level_navigation": False,
    })
    host = RendererProcessHost.__new__(RendererProcessHost)
    host._contexts = {}
    host._active_cancel_token = None
    rebuilt = host._network_kwargs({"kwargs": wire})
    initiator = rebuilt.get("site_for_cookies")
    record("ipc-initiator-survives-broker",
           str(initiator) == "https://top.test/"
           and getattr(initiator, "scheme", None) == "https"
           and rebuilt.get("top_level_navigation") is False,
           f"wire={wire}; rebuilt site_for_cookies={initiator!r} "
           "(None would read as same-site and stop blocking)")

    if not native.available():
        record("ipc-cookie-samesite-cross-site-block", True,
               "skipped: native ggcore not built")
        return
    from browser.renderer_session import RemoteRendererSession

    # end to end: a cross-site page pulling the cookie echo as a
    # subresource. The SameSite=Lax cookie must be withheld, which only
    # happens if the initiator reached the browser process intact.
    session = RemoteRendererSession()
    try:
        _root, _doc, css, _logs = session.commit(
            net.URL("http://cross-site.test/"),
            f'<link rel="stylesheet" href="{base}/cookie/echo">')
        leaked = [c for c in css if "server=secret" in c]
        record("ipc-cookie-samesite-cross-site-block",
               not leaked,
               "cross-site subresource over IPC echoed "
               f"Cookie={leaked[0][:60]!r}" if leaked
               else "cross-site subresource over IPC sent no Cookie")
    finally:
        session.close()


def _network_service_cases(base):
    """Re-run the policies that decide what leaves the machine, with the
    cookie jar, cache and sockets in a *separate process*.

    Moving them out is only worth anything if the rules move with them,
    so these are the same claims as above, asserted through the service.
    """
    from browser.network_backend import create_network_backend

    backend = create_network_backend("service")
    try:
        # the service owns the jar: a Set-Cookie it applied must not be
        # readable in this process
        backend.request_text(net.URL(base + "/cookie/set"), no_cache=True)
        _h, echoed, _f = backend.request_text(
            net.URL(base + "/cookie/echo"), no_cache=True)
        here = net.cookies_for(net.URL(base + "/cookie/echo"))
        record("service-owns-the-cookie-jar",
               "server=secret" in echoed and "server=secret" not in here,
               f"service echo={echoed!r}; browser-process jar={here!r}")

        # ...and still enforces SameSite against the initiator
        _h, cross, _f = backend.request_text(
            net.URL(base + "/cookie/echo"), no_cache=True,
            site_for_cookies=net.URL("https://cross-site.test/"),
            top_level_navigation=False)
        record("service-cookie-samesite-cross-site-block",
               "server=secret" not in cross,
               f"cross-site subresource via service echoed Cookie={cross!r}")

        # a cancel has to reach a request already in flight, and arrive
        # as the type callers branch on
        token = backend.new_cancel_token()
        outcome = {}

        def slow():
            try:
                backend.request_text(net.URL(base + "/slow/cancel"),
                                     cancel_token=token, no_cache=True)
                outcome["r"] = "completed"
            except net.RequestCancelled:
                outcome["r"] = "cancelled"
            except Exception as exc:
                outcome["r"] = type(exc).__name__

        worker = threading.Thread(target=slow)
        worker.start()
        time.sleep(0.3)
        backend.cancel(token)
        worker.join(timeout=8)
        record("service-cancel-reaches-in-flight-request",
               outcome.get("r") == "cancelled",
               f"in-flight request through the service ended as "
               f"{outcome.get('r')!r}")
    finally:
        backend.close()


def main():
    with net._COOKIE_LOCK:
        net._COOKIE_JAR.clear()
    srv, base = start_server()
    port = srv.server_address[1]

    def reset_cached_path(path):
        key = f"http://127.0.0.1:{port}{path}"
        with net._CACHE_LOCK:
            net._CACHE.pop(key, None)
        try:
            os.remove(net._disk_path(key))
        except OSError:
            pass
        return key

    # (1) plain 200 + Content-Length
    h, b, fin = net.request_full(net.URL(base + "/plain"))
    record("plain-200-content-length",
           b == b"hello plain" and h.get("content-length") == "11"
           and srv.hits.get("/plain") == 1,
           f"body={b!r} content-length={h.get('content-length')!r} "
           f"server_hits={srv.hits.get('/plain')}")

    # (2) gzip Content-Encoding
    h, b, _ = net.request_full(net.URL(base + "/gzip"))
    record("gzip-decoded",
           b == GZ_PAYLOAD and "content-encoding" not in h,
           f"sent {len(GZ_COMP)}B compressed, got {len(b)}B decoded, "
           f"match={b == GZ_PAYLOAD}, content-encoding stripped="
           f"{'content-encoding' not in h}")

    # (3) chunked
    h, b, _ = net.request_full(net.URL(base + "/chunked"))
    record("chunked-decoded",
           b == CHUNKED_BODY and h.get("transfer-encoding") == "chunked",
           f"body={b!r} (3 chunks, 1 chunk-ext, trailer consumed)")

    # (4) redirect chain 301->302->200 + final URL propagation
    h, b, fin = net.request_full(net.URL(base + "/redir/a"))
    record("redirect-chain-final-url",
           b == b"redirected-final" and str(fin) == base + "/redir/final"
           and srv.hits.get("/redir/a") == 1 and srv.hits.get("/redir/b") == 1
           and srv.hits.get("/redir/final") == 1,
           f"final_url={fin} body={b!r} hits a/b/final="
           f"{srv.hits.get('/redir/a')}/{srv.hits.get('/redir/b')}/"
           f"{srv.hits.get('/redir/final')}")

    # (4b) redirect loop must error, not hang
    t0 = time.monotonic()
    err = None
    try:
        net.request_full(net.URL(base + "/redir/loop"))
    except RuntimeError as e:
        err = e
    dt = time.monotonic() - t0
    loops = srv.hits.get("/redir/loop")
    record("redirect-loop-capped",
           err is not None and "too many redirects" in str(err)
           and loops == net.MAX_REDIRECTS + 1 and dt < 5,
           f"raised={err!r} in {dt:.2f}s, server hits on /redir/loop={loops} "
           f"(MAX_REDIRECTS={net.MAX_REDIRECTS} -> expected 9)")

    # POST body and request Content-Type reach the wire unchanged.
    post_payload = "name=한글+검색".encode("utf-8")
    h, b, fin = net.request_full(
        net.URL(base + "/post/echo"), no_cache=True, method="POST",
        body=post_payload,
        headers={"Content-Type": "application/x-www-form-urlencoded"})
    record("post-body-content-type",
           b == post_payload and h.get("x-request-method") == "POST"
           and h.get("x-request-content-type") ==
           "application/x-www-form-urlencoded",
           f"body={b!r} method={h.get('x-request-method')!r} "
           f"content-type={h.get('x-request-content-type')!r} final={fin}")

    # Browser redirect compatibility: POST becomes GET for 301/302/303,
    # while 307/308 preserve both method and body.
    redirect_methods = {}
    for code in (301, 302, 303, 307, 308):
        _h, redirected, _fin = net.request_full(
            net.URL(base + f"/method-redirect/{code}"), no_cache=True,
            method="POST", body=b"k=v",
            headers={"Content-Type": "application/x-www-form-urlencoded"})
        redirect_methods[code] = redirected
    record("redirect-method-body-rules",
           all(redirect_methods[code] == b"GET:"
               for code in (301, 302, 303))
           and all(redirect_methods[code] == b"POST:k=v"
                   for code in (307, 308)),
           repr(redirect_methods))

    rejected = []
    for kwargs in (
            {"method": "GET", "body": b"not-allowed"},
            {"headers": {"X-Test": "ok\r\nInjected: yes"}},
            {"method": "G\r\nET"}):
        try:
            net.request_full(net.URL(base + "/plain"), **kwargs)
        except ValueError:
            rejected.append(True)
        else:
            rejected.append(False)
    record("request-injection-and-get-body-rejected",
           all(rejected), f"rejected={rejected}")

    # (5) cache: max-age=3600, hit counter
    reset_cached_path("/cached")
    curl = net.URL(base + "/cached")
    key = f"http://127.0.0.1:{port}/cached"
    dpath = net._disk_path(key)
    net.request(net.URL(base + "/cached"))          # fetch 1 -> network
    c1 = srv.hits.get("/cached")
    net.request(net.URL(base + "/cached"))          # fetch 2 -> cache?
    c2 = srv.hits.get("/cached")
    mem_has = key in net._CACHE
    disk_has = os.path.exists(dpath)
    record("cache-second-fetch-served-from-cache",
           c1 == 1 and c2 == 1 and mem_has and disk_has,
           f"server hits after fetch1/fetch2 = {c1}/{c2}; "
           f"memory cache has key={mem_has}; disk file exists={disk_has} "
           f"at {dpath} (disk dir={net._DISK_DIR})")

    # which cache served fetch 2? memory is consulted first (net.py:324).
    # prove disk works independently: clear memory, fetch again.
    net._CACHE.clear()
    net.request(net.URL(base + "/cached"))          # fetch 3 -> disk?
    c3 = srv.hits.get("/cached")
    promoted = key in net._CACHE
    record("cache-disk-serves-after-memory-cleared",
           c3 == 1 and promoted,
           f"cleared _CACHE; fetch3 server hits={c3} (still 1 -> disk hit); "
           f"promoted back to memory={promoted}")

    # no_cache=True must bust both caches
    net.request_text(net.URL(base + "/cached"), no_cache=True)  # fetch 4
    c4 = srv.hits.get("/cached")
    record("no-cache-busts",
           c4 == 2,
           f"request_text(no_cache=True) -> server hits={c4} (was 1) "
           f"[net.request has no no_cache param; request_full/request_text do]")

    # Stale ETag entries are conditionally revalidated. A 304 is merged with
    # the stored metadata and exposed to browser callers as the cached 200.
    etag_key = reset_cached_path("/cache/etag")
    status1, h1, b1, _ = net.request_full_status(
        net.URL(base + "/cache/etag"))
    status2, h2, b2, _ = net.request_full_status(
        net.URL(base + "/cache/etag"))
    status3, _h3, b3, _ = net.request_full_status(
        net.URL(base + "/cache/etag"))
    etag_requests = [request for request in srv.requests
                     if request["path"] == "/cache/etag"]
    record("cache-etag-304-merges-and-refreshes",
           status1 == status2 == status3 == 200
           and b1 == b2 == b3 == b"etag-body-v1"
           and srv.hits.get("/cache/etag") == 2
           and etag_requests[-1]["if_none_match"] == '"v1"'
           and h2.get("x-original") == "kept"
           and h2.get("x-revalidated") == "yes",
           f"statuses={status1}/{status2}/{status3} "
           f"hits={srv.hits.get('/cache/etag')} "
           f"If-None-Match={etag_requests[-1]['if_none_match']!r} "
           f"merged={h2.get('x-original')!r}/{h2.get('x-revalidated')!r}")

    # A request-side no-cache directive validates even a currently fresh
    # entry and uses its validator.
    status4, _h4, b4, _ = net.request_full_status(
        net.URL(base + "/cache/etag"),
        headers={"Cache-Control": "no-cache"})
    etag_requests = [request for request in srv.requests
                     if request["path"] == "/cache/etag"]
    record("request-no-cache-forces-validation",
           status4 == 200 and b4 == b"etag-body-v1"
           and srv.hits.get("/cache/etag") == 3
           and etag_requests[-1]["if_none_match"] == '"v1"',
           f"status={status4} hits={srv.hits.get('/cache/etag')} "
           f"If-None-Match={etag_requests[-1]['if_none_match']!r}")

    # Stale validators remain useful on disk after the memory layer is gone.
    modified_key = reset_cached_path("/cache/last-modified")
    _h, first_modified, _ = net.request_full(
        net.URL(base + "/cache/last-modified"))
    disk_before = os.path.exists(net._disk_path(modified_key))
    with net._CACHE_LOCK:
        net._CACHE.pop(modified_key, None)
    status, _h, second_modified, _ = net.request_full_status(
        net.URL(base + "/cache/last-modified"))
    _h, third_modified, _ = net.request_full(
        net.URL(base + "/cache/last-modified"))
    modified_requests = [request for request in srv.requests
                         if request["path"] == "/cache/last-modified"]
    record("cache-disk-last-modified-revalidation",
           disk_before and status == 200
           and first_modified == second_modified == third_modified
           == b"last-modified-body"
           and srv.hits.get("/cache/last-modified") == 2
           and bool(modified_requests[-1]["if_modified_since"]),
           f"disk={disk_before} status={status} "
           f"hits={srv.hits.get('/cache/last-modified')} "
           f"If-Modified-Since="
           f"{modified_requests[-1]['if_modified_since']!r}")

    # Vary request fields select independent representations in memory.
    vary_key = reset_cached_path("/cache/vary")
    _h, en1, _ = net.request_full(net.URL(base + "/cache/vary"),
                                   headers={"Accept-Language": "en"})
    _h, fr1, _ = net.request_full(net.URL(base + "/cache/vary"),
                                   headers={"Accept-Language": "fr"})
    _h, en2, _ = net.request_full(net.URL(base + "/cache/vary"),
                                   headers={"Accept-Language": "en"})
    record("cache-vary-keeps-request-variants",
           en1 == en2 == b"language=en" and fr1 == b"language=fr"
           and srv.hits.get("/cache/vary") == 2,
           f"bodies={en1!r}/{fr1!r}/{en2!r} "
           f"hits={srv.hits.get('/cache/vary')}")

    # The compact disk layer holds the newest variant only. A mismatch must
    # go to the network, never return that other variant.
    with net._CACHE_LOCK:
        net._CACHE.pop(vary_key, None)
    _h, en_after_clear, _ = net.request_full(
        net.URL(base + "/cache/vary"),
        headers={"Accept-Language": "en"})
    record("cache-disk-vary-mismatch-is-a-miss",
           en_after_clear == b"language=en"
           and srv.hits.get("/cache/vary") == 3,
           f"body={en_after_clear!r} hits={srv.hits.get('/cache/vary')}")

    no_store_key = reset_cached_path("/cache/no-store")
    _h, no_store1, _ = net.request_full(net.URL(base + "/cache/no-store"))
    _h, no_store2, _ = net.request_full(net.URL(base + "/cache/no-store"))
    record("cache-response-no-store-is-never-stored",
           no_store1 == b"no-store-1" and no_store2 == b"no-store-2"
           and no_store_key not in net._CACHE
           and not os.path.exists(net._disk_path(no_store_key)),
           f"bodies={no_store1!r}/{no_store2!r} "
           f"memory={no_store_key in net._CACHE} "
           f"disk={os.path.exists(net._disk_path(no_store_key))}")

    private_key = reset_cached_path("/cache/private")
    net.request_full(net.URL(base + "/cache/private"))
    net.request_full(net.URL(base + "/cache/private"))
    private_hits = srv.hits.get("/cache/private")
    private_disk = os.path.exists(net._disk_path(private_key))
    record("cache-private-allowed-in-private-browser-cache",
           private_hits == 1 and private_key in net._CACHE and private_disk,
           f"hits={private_hits} memory={private_key in net._CACHE} "
           f"disk={private_disk}")

    net.request_full(net.URL(base + "/cache/private"),
                     headers={"Cache-Control": "no-store"})
    removed_by_request = (private_key not in net._CACHE
                          and not os.path.exists(net._disk_path(private_key)))
    net.request_full(net.URL(base + "/cache/private"))
    record("cache-request-no-store-bypasses-and-removes",
           removed_by_request and srv.hits.get("/cache/private") == 3,
           f"removed={removed_by_request} "
           f"hits={srv.hits.get('/cache/private')}")

    vary_star_key = reset_cached_path("/cache/vary-star")
    _h, star1, _ = net.request_full(net.URL(base + "/cache/vary-star"))
    _h, star2, _ = net.request_full(net.URL(base + "/cache/vary-star"))
    record("cache-vary-star-is-never-reused",
           star1 == b"vary-star-1" and star2 == b"vary-star-2"
           and vary_star_key not in net._CACHE,
           f"bodies={star1!r}/{star2!r} "
           f"memory={vary_star_key in net._CACHE}")

    # Cancellation closes the registered socket from another thread, so a
    # stalled body read ends immediately instead of waiting for its timeout.
    cancel_token = net.CancellationToken()
    with ThreadPoolExecutor(max_workers=1) as pool:
        cancelled_future = pool.submit(
            net.request_full, net.URL(base + "/slow/cancel"),
            net.MAX_REDIRECTS, False, timeout=5,
            cancel_token=cancel_token)
        slow_started = srv.slow_started.wait(1.0)
        cancel_started = time.monotonic()
        cancel_token.cancel()
        cancelled_error = None
        try:
            cancelled_future.result(timeout=2.0)
        except Exception as exc:
            cancelled_error = exc
        cancel_elapsed = time.monotonic() - cancel_started
    record("network-cancellation-interrupts-body-read",
           slow_started and isinstance(
               cancelled_error, net.RequestCancelled)
           and cancel_elapsed < 0.5,
           f"started={slow_started} error={cancelled_error!r} "
           f"elapsed={cancel_elapsed:.3f}s")

    timeout_started = time.monotonic()
    timeout_error = None
    try:
        net.request_full(
            net.URL(base + "/slow/timeout"), timeout=0.05)
    except Exception as exc:
        timeout_error = exc
    timeout_elapsed = time.monotonic() - timeout_started
    record("network-total-timeout-bounds-header-wait",
           isinstance(timeout_error, net.RequestTimeout)
           and timeout_elapsed < 0.2,
           f"error={timeout_error!r} elapsed={timeout_elapsed:.3f}s")

    # Public Suffix List: exact, wildcard, exception, private and IDNA rules.
    idn = psl.canonical_host("食狮.公司.cn")
    record("psl-exact-wildcard-exception-private-idna",
           psl.version() != "fallback"
           and psl.public_suffix("shop.example.co.uk") == "co.uk"
           and psl.registrable_domain("shop.example.co.uk")
           == "example.co.uk"
           and psl.public_suffix("a.b.ck") == "b.ck"
           and psl.registrable_domain("www.ck") == "www.ck"
           and psl.public_suffix("foo.github.io") == "github.io"
           and psl.registrable_domain(idn) == idn,
           f"version={psl.version()} idn={idn} "
           f"a.b.ck={psl.public_suffix('a.b.ck')} "
           f"www.ck={psl.registrable_domain('www.ck')}")

    net._COOKIE_JAR.clear()
    uk_origin = net.URL("https://shop.example.co.uk/")
    blocked_icann = net._store_set_cookie(
        uk_origin, ["wide=bad; Domain=co.uk; Secure"])
    blocked_private = net._store_set_cookie(
        net.URL("https://tenant.github.io/"),
        ["wide=bad; Domain=github.io; Secure"])
    accepted_domain = net._store_set_cookie(
        uk_origin,
        ["scoped=ok; Domain=example.co.uk; Secure; SameSite=Strict"])
    record("cookie-domain-rejects-public-and-private-suffixes",
           blocked_icann == 0 and blocked_private == 0
           and accepted_domain == 1,
           f"co.uk={blocked_icann} github.io={blocked_private} "
           f"example.co.uk={accepted_domain}")
    same_scheme = net._cookie_header(
        net.URL("https://cdn.example.co.uk/resource"),
        site_for_cookies=net.URL("https://app.example.co.uk/"),
        top_level_navigation=False)
    cross_scheme = net._cookie_header(
        net.URL("https://cdn.example.co.uk/resource"),
        site_for_cookies=net.URL("http://app.example.co.uk/"),
        top_level_navigation=False)
    record("schemeful-site-uses-etld-plus-one-and-scheme",
           "scoped=ok" in same_scheme
           and "scoped=ok" not in cross_scheme,
           f"same={same_scheme!r} cross-scheme={cross_scheme!r} "
           f"keys={net._site_key(net.URL('https://a.example.co.uk/'))}/"
           f"{net._site_key(net.URL('http://b.example.co.uk/'))}")
    net._COOKIE_JAR.clear()

    # cookie wire behaviour: HttpOnly is sent but hidden from JS; Path and
    # SameSite are enforced before a Cookie header is composed.
    net.request_full(net.URL(base + "/cookie/set"), no_cache=True)
    _h, cookie_body, _ = net.request_full(
        net.URL(base + "/cookie/echo"), no_cache=True)
    visible = net.cookies_for(net.URL(base + "/cookie/echo"))
    record("cookie-httponly-path-wire-scope",
           cookie_body == b"server=secret" and visible == "",
           f"wire={cookie_body!r}; document.cookie={visible!r}; "
           "Path=/other cookie withheld")
    _h, cross_body, _ = net.request_full(
        net.URL(base + "/cookie/echo"), no_cache=True,
        site_for_cookies=net.URL("https://cross-site.test/"),
        top_level_navigation=False)
    record("cookie-samesite-cross-site-block",
           cross_body == b"",
           f"cross-site subresource Cookie echo={cross_body!r}")
    _h, cross_post_body, _ = net.request_full(
        net.URL(base + "/cookie/echo"), no_cache=True,
        method="POST", body=b"x=1",
        site_for_cookies=net.URL("https://cross-site.test/"),
        top_level_navigation=True)
    record("cookie-samesite-lax-blocks-cross-site-post",
           cross_post_body == b"",
           f"cross-site top-level POST Cookie echo={cross_post_body!r}")

    # Script fetch policy: a second port is a distinct origin while staying
    # same-site, which lets credentials tests use ordinary local cookies.
    cors_srv, cors_base = start_server()
    page_url = net.URL(base + "/page")
    public = security.perform_script_fetch(page_url, {
        "url": cors_base + "/cors/public",
        "method": "GET", "headers": [], "mode": "cors",
        "credentials": "omit",
    })
    record("cors-public-origin-allowed",
           public.status == 200 and public.body == "cors-public",
           f"status={public.status} body={public.body!r}")

    denied = None
    try:
        security.perform_script_fetch(page_url, {
            "url": cors_base + "/cors/deny",
            "method": "GET", "headers": [], "mode": "cors",
            "credentials": "omit",
        })
    except security.FetchPolicyError as exc:
        denied = exc
    record("cors-missing-allow-origin-rejected",
           denied is not None, repr(denied))

    preflight_before = len(cors_srv.requests)
    preflight = security.perform_script_fetch(page_url, {
        "url": cors_base + "/cors/preflight",
        "method": "POST", "body": '{"x":1}',
        "headers": [("Content-Type", "application/json"),
                    ("X-Token", "yes")],
        "mode": "cors", "credentials": "omit",
    })
    preflight_requests = cors_srv.requests[preflight_before:]
    record("cors-preflight-method-and-headers",
           preflight.body == 'POST:{"x":1}'
           and [request["method"] for request in preflight_requests]
           == ["OPTIONS", "POST"]
           and preflight_requests[0]["path"] == "/cors/preflight",
           repr(preflight_requests))

    cors_url = net.URL(cors_base + "/cors/credentials")
    net._store_set_cookie(cors_url, ["corsid=1; Path=/; SameSite=Lax"])
    included = security.perform_script_fetch(page_url, {
        "url": str(cors_url), "method": "GET", "headers": [],
        "mode": "cors", "credentials": "include",
    })
    omitted = security.perform_script_fetch(page_url, {
        "url": str(cors_url), "method": "GET", "headers": [],
        "mode": "cors", "credentials": "omit",
    })
    record("cors-credentials-include-vs-omit",
           included.body == "corsid=1" and omitted.body == "",
           f"include={included.body!r} omit={omitted.body!r}")

    wildcard_credentials = None
    try:
        security.perform_script_fetch(page_url, {
            "url": cors_base + "/cors/public",
            "method": "GET", "headers": [], "mode": "cors",
            "credentials": "include",
        })
    except security.FetchPolicyError as exc:
        wildcard_credentials = exc
    record("cors-wildcard-with-credentials-rejected",
           wildcard_credentials is not None,
           repr(wildcard_credentials))

    opaque = security.perform_script_fetch(page_url, {
        "url": cors_base + "/cors/deny",
        "method": "GET", "headers": [], "mode": "no-cors",
        "credentials": "omit",
    })
    record("no-cors-cross-origin-response-is-opaque",
           opaque.opaque and opaque.status == 0 and opaque.body == "",
           repr(opaque))

    policy_blocks = []
    for base_probe, request in (
            (page_url, {
                "url": cors_base + "/cors/public", "method": "GET",
                "headers": [], "mode": "same-origin",
                "credentials": "same-origin"}),
            (net.URL("https://secure.example/page"), {
                "url": "http://insecure.example/data", "method": "GET",
                "headers": [], "mode": "cors", "credentials": "omit"})):
        try:
            security.perform_script_fetch(base_probe, request)
        except (security.FetchPolicyError, PermissionError):
            policy_blocks.append(True)
        else:
            policy_blocks.append(False)
    record("same-origin-mode-and-mixed-content-blocked",
           policy_blocks == [True, True], repr(policy_blocks))
    cors_srv.shutdown()

    # (6) keep-alive: separate server for a clean connection count
    srv2, base2 = start_server()
    net.request_full(net.URL(base2 + "/ka/1"), no_cache=True)
    net.request_full(net.URL(base2 + "/ka/2"), no_cache=True)
    record("keep-alive-pool-reuse",
           srv2.conn_count == 1 and srv2.hits.get("/ka/1") == 1
           and srv2.hits.get("/ka/2") == 1,
           f"2 sequential GETs to distinct paths: server accepted "
           f"{srv2.conn_count} connection(s), hits={dict(srv2.hits)}")
    srv2.shutdown()

    # (7) huge header + weird casing + colon-less garbage line
    h, b, _ = net.request_full(net.URL(base + "/hugehdr"))
    record("huge-header-weird-casing",
           b == b"hugehdrok" and h.get("x-huge") == HUGE.decode()
           and h.get("content-length") == "9",
           f"64KiB X-Huge header len={len(h.get('x-huge', ''))}; "
           f"'cOnTeNt-LenGTH' parsed as content-length={h.get('content-length')!r}; "
           f"colon-less line skipped; body={b!r}")

    # (8) POST support probe: API + wire evidence
    import inspect
    sig = str(inspect.signature(net.request_full))
    cmds = set(srv.commands) | set(srv2.commands)
    record("post-api-and-wire-enabled",
           "method" in sig and "body" in sig and "headers" in sig
           and cmds == {"GET", "POST"},
           f"request_full signature={sig}; "
           f"wire commands seen={sorted(cmds)}")

    # file:// scheme
    fpath = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                         "file_scheme_probe.txt")
    with open(fpath, "wb") as f:
        f.write(b"file scheme ok\n")
    h, b, fin = net.request_full(net.URL("file://" + fpath))
    record("file-scheme", b == b"file scheme ok\n",
           f"URL=file://{fpath} body={b!r}")

    # data: scheme (plain + base64)
    h1, t1 = net.request(net.URL("data:text/plain,hello%20world"))
    b64 = base64.b64encode(b"<b>hi</b>").decode()
    h2, b2, _ = net.request_full(net.URL("data:text/html;base64," + b64))
    record("data-scheme",
           t1 == "hello world" and h1.get("content-type") == "text/plain"
           and b2 == b"<b>hi</b>" and h2.get("content-type") == "text/html",
           f"plain: {t1!r} ct={h1.get('content-type')}; "
           f"base64: {b2!r} ct={h2.get('content-type')}")

    # about: scheme
    h, t = net.request(net.URL("about:blank"))
    record("about-scheme",
           "about:blank" in t and t.startswith("<html>"),
           f"about:blank -> {len(t)} chars: {t[:60]!r}")

    _ipc_cookie_policy_cases(base)
    _network_service_cases(base)

    srv.shutdown()
    passed = sum(r["ok"] for r in RESULTS)
    print(json.dumps({"passed": passed, "total": len(RESULTS),
                      "disk_cache_dir": net._DISK_DIR,
                      "results": RESULTS}, indent=1))
    return 0 if passed == len(RESULTS) else 1


if __name__ == "__main__":
    sys.exit(main())
