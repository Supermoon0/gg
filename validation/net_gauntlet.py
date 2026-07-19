#!/usr/bin/env python3
"""Gauntlet for browser/net.py against a local http.server.

Run: python3 validation/net_gauntlet.py (repo root auto-detected)
Prints one PASS/FAIL line per claim, then a JSON summary.
"""
import base64
import gzip as gzmod
import json
import os
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from browser import net  # noqa: E402

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
        self.lock = threading.Lock()


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

    def do_GET(self):
        path = self.path.split("?")[0]
        with self.server.lock:
            self.server.hits[path] = self.server.hits.get(path, 0) + 1
            self.server.commands.append(self.command)

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
        elif path == "/redir/loop":
            self._body(301, b"", [("Location", "/redir/loop")])
        elif path == "/cached":
            self._body(200, b"cacheable-payload",
                       [("Cache-Control", "max-age=3600")])
        elif path.startswith("/ka/"):
            self._body(200, ("ka:" + path).encode())
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

    do_POST = do_GET


def start_server():
    srv = Server(("127.0.0.1", 0), Handler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv, f"http://127.0.0.1:{srv.server_address[1]}"


def main():
    srv, base = start_server()
    port = srv.server_address[1]

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

    # (5) cache: max-age=3600, hit counter
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
    record("post-unsupported-get-only",
           "method" not in sig and cmds == {"GET"},
           f"request_full signature={sig} (no method/body params); "
           f"_one_request hardcodes 'GET'; wire commands seen={sorted(cmds)}; "
           f"forms.py:59 returns None for method=post ('GET-only forms')")

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

    srv.shutdown()
    passed = sum(r["ok"] for r in RESULTS)
    print(json.dumps({"passed": passed, "total": len(RESULTS),
                      "disk_cache_dir": net._DISK_DIR,
                      "results": RESULTS}, indent=1))
    return 0 if passed == len(RESULTS) else 1


if __name__ == "__main__":
    sys.exit(main())
