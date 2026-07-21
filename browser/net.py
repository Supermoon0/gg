"""Network layer: URL parsing and HTTP/HTTPS fetching.

HTTP/1.1 with persistent connections (per-host socket pool), an
in-memory response cache honoring Cache-Control, TLS, redirects,
chunked transfer encoding, gzip, and file:/about:/data: schemes.
"""

import base64
import gzip
import hashlib
import json
import os
import socket
import ssl
import tempfile
import threading
import time
import urllib.parse

USER_AGENT = "GGBrowser/0.1 (educational engine)"
MAX_REDIRECTS = 8

# --- cookie jar (host -> {name: value}) ---
# A minimal session cookie store: Set-Cookie response headers are captured
# here and replayed as a Cookie request header to the same host. Attributes
# (Path/Domain/Expires/Secure) are ignored — enough for session continuity
# across a page's requests and for document.cookie round-tripping.
_COOKIE_JAR = {}


def _cookie_header(host):
    jar = _COOKIE_JAR.get(host)
    if not jar:
        return ""
    pairs = "; ".join(f"{k}={v}" for k, v in jar.items())
    return f"Cookie: {pairs}\r\n"


def _store_set_cookie(host, values):
    """Store one or more Set-Cookie header values (name=value; attrs...)."""
    jar = _COOKIE_JAR.setdefault(host, {})
    for sc in values:
        first = sc.split(";", 1)[0].strip()
        if "=" in first:
            k, v = first.split("=", 1)
            k = k.strip()
            if k:
                jar[k] = v.strip()


def cookies_for(host):
    """The `document.cookie` string for a host: `k=v; k2=v2`."""
    jar = _COOKIE_JAR.get(host)
    if not jar:
        return ""
    return "; ".join(f"{k}={v}" for k, v in jar.items())


def set_cookie_from_js(host, cookie_str):
    """Apply a `document.cookie = 'k=v; Path=/'` write to the jar."""
    _store_set_cookie(host, [cookie_str])

# --- connection pool ---
_POOL = {}
_POOL_LOCK = threading.Lock()
_POOL_MAX_PER_HOST = 6

# --- response cache ---
_CACHE = {}
_CACHE_LOCK = threading.Lock()
_CACHE_MAX_ENTRIES = 300
_CACHE_MAX_BODY = 4 * 1024 * 1024
_CACHE_DEFAULT_TTL = 300.0

# --- disk cache (survives restarts; only explicit max-age responses,
# so an asset CDN like pstatic hits disk while HTML stays fresh) ---
_DISK_DIR = os.path.join(
    os.environ.get("LOCALAPPDATA") or tempfile.gettempdir(),
    "gg-browser", "cache")
_DISK_MAX_BODY = 8 * 1024 * 1024
_DISK_MAX_TTL = 7 * 24 * 3600.0


def _pool_get(key):
    with _POOL_LOCK:
        conns = _POOL.get(key)
        if conns:
            return conns.pop()
    return None


def _pool_put(key, s):
    with _POOL_LOCK:
        conns = _POOL.setdefault(key, [])
        if len(conns) < _POOL_MAX_PER_HOST:
            conns.append(s)
            return
    try:
        s.close()
    except OSError:
        pass


def _env_proxy(scheme):
    """(host, port) of the environment proxy for this scheme, or None.
    Managed/cloud environments route egress through HTTP(S)_PROXY;
    without this the engine's raw sockets bypass the proxy and get
    blocked by the network policy."""
    var = "https_proxy" if scheme == "https" else "http_proxy"
    val = os.environ.get(var) or os.environ.get(var.upper())
    if not val:
        return None
    p = urllib.parse.urlsplit(val if "://" in val else "//" + val)
    if not p.hostname:
        return None
    return p.hostname, p.port or 3128


def _no_proxy(host):
    val = os.environ.get("no_proxy") or os.environ.get("NO_PROXY") or ""
    host = host.lower()
    for entry in val.split(","):
        entry = entry.strip().lower()
        if not entry:
            continue
        if entry == "*":
            return True
        entry = entry.lstrip("*").lstrip(".")
        if host == entry or host.endswith("." + entry):
            return True
    return False


def _connect(url):
    proxy = None if _no_proxy(url.host) else _env_proxy(url.scheme)
    if proxy is None:
        # create_connection resolves IPv4 and IPv6 alike
        s = socket.create_connection((url.host, url.port), timeout=15)
        if url.scheme == "https":
            ctx = ssl.create_default_context()
            s = ctx.wrap_socket(s, server_hostname=url.host)
        return s

    s = socket.create_connection(proxy, timeout=15)
    if url.scheme == "https":
        # CONNECT tunnel, then TLS to the origin through it
        target = f"[{url.host}]" if ":" in url.host else url.host
        s.sendall((f"CONNECT {target}:{url.port} HTTP/1.1\r\n"
                   f"Host: {target}:{url.port}\r\n\r\n").encode("ascii"))
        head = b""
        while b"\r\n\r\n" not in head:
            chunk = s.recv(4096)
            if not chunk:
                raise ConnectionError("proxy closed during CONNECT")
            head += chunk
            if len(head) > 65536:
                raise ConnectionError("oversized CONNECT response")
        statusline = head.split(b"\r\n", 1)[0].decode("latin-1")
        parts = statusline.split(" ", 2)
        code = int(parts[1]) if len(parts) >= 2 and parts[1].isdigit() else 0
        if code != 200:
            raise ConnectionError(f"proxy CONNECT failed: {statusline}")
        ctx = ssl.create_default_context()
        s = ctx.wrap_socket(s, server_hostname=url.host)
    else:
        # plain http: speak to the proxy directly (absolute-form target)
        s._gg_absolute_form = True
    return s


def _cache_get(key):
    with _CACHE_LOCK:
        entry = _CACHE.get(key)
        if entry and entry[0] > time.monotonic():
            return entry[1], entry[2]
        if entry:
            del _CACHE[key]
    return None


def _cache_put(key, headers, body):
    cc = headers.get("cache-control", "").casefold()
    if "no-store" in cc or "no-cache" in cc or len(body) > _CACHE_MAX_BODY:
        return
    ttl = _CACHE_DEFAULT_TTL
    if "max-age=" in cc:
        try:
            ttl = min(float(cc.split("max-age=", 1)[1].split(",")[0]),
                      3600.0)
        except ValueError:
            pass
    if ttl <= 0:
        return
    with _CACHE_LOCK:
        if len(_CACHE) >= _CACHE_MAX_ENTRIES:
            oldest = min(_CACHE, key=lambda k: _CACHE[k][0])
            del _CACHE[oldest]
        _CACHE[key] = (time.monotonic() + ttl, headers, body)


def _disk_path(key):
    digest = hashlib.sha256(key.encode("utf-8")).hexdigest()[:32]
    return os.path.join(_DISK_DIR, digest)


def _disk_get(key):
    path = _disk_path(key)
    try:
        with open(path, "rb") as f:
            meta = json.loads(f.readline())
            if meta["expires"] < time.time():
                raise ValueError("expired")
            return meta["headers"], f.read()
    except (OSError, ValueError, KeyError):
        try:
            os.remove(path)
        except OSError:
            pass
        return None


def _disk_put(key, headers, body):
    cc = headers.get("cache-control", "").casefold()
    if ("no-store" in cc or "no-cache" in cc or "private" in cc
            or "max-age=" not in cc or len(body) > _DISK_MAX_BODY):
        return
    try:
        ttl = float(cc.split("max-age=", 1)[1].split(",")[0])
    except ValueError:
        return
    if ttl <= 0:
        return
    meta = json.dumps({
        "expires": time.time() + min(ttl, _DISK_MAX_TTL),
        "headers": headers,
    })
    try:
        os.makedirs(_DISK_DIR, exist_ok=True)
        fd, tmp = tempfile.mkstemp(dir=_DISK_DIR)
        with os.fdopen(fd, "wb") as f:
            f.write(meta.encode("utf-8"))
            f.write(b"\n")
            f.write(body)
        os.replace(tmp, _disk_path(key))
    except OSError:
        pass


class URL:
    def __init__(self, url):
        self.original = url
        self.fragment = None
        if "#" in url:
            url, self.fragment = url.split("#", 1)

        if url.startswith("about:"):
            self.scheme = "about"
            self.host = ""
            self.port = 0
            self.path = url[len("about:"):]
            return

        if url.startswith("data:"):
            self.scheme = "data"
            self.host = ""
            self.port = 0
            self.path = url[len("data:"):]
            return

        self.scheme, rest = url.split("://", 1)
        self.scheme = self.scheme.lower()
        assert self.scheme in ("http", "https", "file"), \
            f"unsupported scheme: {self.scheme}"

        if self.scheme == "file":
            self.host = ""
            self.port = 0
            self.path = rest
            return

        # authority ends at the first '/' or '?' (query with no path)
        slash = rest.find("/")
        q = rest.find("?")
        if q != -1 and (slash == -1 or q < slash):
            authority, self.path = rest[:q], "/" + rest[q:]
        elif slash != -1:
            authority, self.path = rest[:slash], rest[slash:]
        else:
            authority, self.path = rest, "/"

        # userinfo is dropped: we never send credentials
        if "@" in authority:
            authority = authority.rsplit("@", 1)[1]

        self.port = 80 if self.scheme == "http" else 443
        if authority.startswith("["):  # IPv6 literal: [::1] or [::1]:8080
            end = authority.find("]")
            if end < 0:
                raise ValueError(f"bad IPv6 host: {authority}")
            self.host = authority[1:end]
            tail = authority[end + 1:]
            if tail.startswith(":"):
                self.port = int(tail[1:])
        else:
            self.host = authority
            if ":" in authority:
                self.host, port = authority.split(":", 1)
                self.port = int(port)
        if not self.host or any(c.isspace() or ord(c) < 0x20
                                for c in self.host):
            raise ValueError(f"bad host: {self.host!r}")

    def __str__(self):
        if self.scheme == "about":
            return "about:" + self.path
        if self.scheme == "data":
            return "data:" + self.path
        if self.scheme == "file":
            return "file://" + self.path
        port = ""
        default = 80 if self.scheme == "http" else 443
        if self.port != default:
            port = ":" + str(self.port)
        host = f"[{self.host}]" if ":" in self.host else self.host
        s = f"{self.scheme}://{host}{port}{self.path}"
        if self.fragment is not None:
            s += "#" + self.fragment
        return s

    def resolve(self, link):
        """Resolve a (possibly relative) link against this URL."""
        link = link.strip()
        if not link:
            return self
        if link.startswith(("data:", "about:")):
            return URL(link)
        if link.startswith("#"):
            copy = URL(str(self))
            copy.fragment = link[1:]
            return copy
        if "://" in link:
            return URL(link)
        if link.startswith("//"):
            return URL(self.scheme + ":" + link)
        if self.scheme in ("about", "file"):
            base = "file:///" if self.scheme == "file" else "about:"
            return URL(urllib.parse.urljoin(base + self.path, link))
        joined = urllib.parse.urljoin(str(self), link)
        return URL(joined)


def request_full(url, max_redirects=MAX_REDIRECTS, no_cache=False):
    """Fetch a URL. Returns (headers: dict, body: bytes, final_url) —
    final_url differs from url after redirects; callers must use it as
    the base for relative links."""
    if url.scheme == "about":
        from .pages import about_page
        return {}, about_page(url.path).encode("utf-8"), url

    if url.scheme == "data":
        headers, body = _data_url(url.path)
        return headers, body, url

    if url.scheme == "file":
        path = url.path
        # a query has no filesystem meaning (form GET submit to file:)
        path = path.split("?", 1)[0].split("#", 1)[0]
        # file:///C:/foo -> C:/foo on Windows
        if len(path) >= 3 and path[0] == "/" and path[2] == ":":
            path = path[1:]
        with open(path, "rb") as f:
            return {}, f.read(), url

    return _http_request(url, max_redirects, no_cache)


def request_raw(url, max_redirects=MAX_REDIRECTS):
    """Fetch a URL. Returns (headers: dict, body: bytes)."""
    headers, body, _ = request_full(url, max_redirects)
    return headers, body


def request_text(url, max_redirects=MAX_REDIRECTS, no_cache=False):
    """Fetch and decode as text. Returns (headers, str, final_url)."""
    headers, body, final_url = request_full(url, max_redirects, no_cache)
    charset = "utf-8"
    ctype = headers.get("content-type", "")
    if "charset=" in ctype:
        charset = ctype.split("charset=", 1)[1].split(";")[0].strip().strip('"')
    try:
        return headers, body.decode(charset, errors="replace"), final_url
    except LookupError:
        return headers, body.decode("utf-8", errors="replace"), final_url


def request(url, max_redirects=MAX_REDIRECTS):
    """Fetch a URL and decode it as text. Returns (headers, str)."""
    headers, text, _ = request_text(url, max_redirects)
    return headers, text


def _data_url(path):
    """data:[<mediatype>][;base64],<data>"""
    if "," not in path:
        return {}, b""
    meta, payload = path.split(",", 1)
    if meta.endswith(";base64"):
        try:
            body = base64.b64decode(payload)
        except Exception:
            body = b""
        meta = meta[:-len(";base64")]
    else:
        body = urllib.parse.unquote(payload).encode("utf-8")
    return {"content-type": meta or "text/plain"}, body


def _http_request(url, redirects_left, no_cache=False):
    # fragments never reach the wire: exclude them from the cache key
    cache_key = f"{url.scheme}://{url.host}:{url.port}{url.path}"
    if not no_cache:
        cached = _cache_get(cache_key)
        if cached is not None:
            headers, body = cached
            return headers, body, url
        disk = _disk_get(cache_key)
        if disk is not None:
            headers, body = disk
            _cache_put(cache_key, headers, body)  # promote to memory
            return headers, body, url

    pool_key = (url.scheme, url.host, url.port)
    last_error = None
    for attempt in range(3):
        s = _pool_get(pool_key)
        reused = s is not None
        if s is None:
            s = _connect(url)
        try:
            result = _one_request(url, s, pool_key)
            break
        except (OSError, ssl.SSLError, ConnectionError) as e:
            try:
                s.close()
            except OSError:
                pass
            last_error = e
            if not reused:  # fresh connection genuinely failed
                raise
    else:
        raise last_error

    status, headers, body = result

    if 300 <= status < 400 and "location" in headers:
        if redirects_left <= 0:
            raise RuntimeError("too many redirects")
        target = url.resolve(headers["location"])
        return _http_request(target, redirects_left - 1, no_cache)

    if headers.get("content-encoding", "").lower() == "gzip":
        body = gzip.decompress(body) if body else b""
        headers = dict(headers)
        del headers["content-encoding"]  # body is stored decoded
    if status == 200:
        _cache_put(cache_key, headers, body)
        _disk_put(cache_key, headers, body)
    return headers, body, url


def _safe_path(path):
    """Request-target with control chars/spaces percent-encoded, so a
    crafted URL can never inject headers into the request."""
    return urllib.parse.quote(path, safe="/?#[]@!$&'()*+,;=:%~._-")


def _one_request(url, s, pool_key):
    """Send one GET on an open socket; returns (status, headers, body).
    Returns the socket to the pool when the response allows reuse."""
    host = f"[{url.host}]" if ":" in url.host else url.host
    default = 80 if url.scheme == "http" else 443
    if url.port != default:
        host += f":{url.port}"  # RFC 7230: Host carries non-default port
    target = _safe_path(url.path)
    if getattr(s, "_gg_absolute_form", False):
        # plain-http via an environment proxy: absolute-form target
        target = f"{url.scheme}://{host}{target}"
    req = (
        f"GET {target} HTTP/1.1\r\n"
        f"Host: {host}\r\n"
        f"Connection: keep-alive\r\n"
        f"User-Agent: {USER_AGENT}\r\n"
        f"Accept: text/html,*/*\r\n"
        f"Accept-Encoding: gzip\r\n"
        f"{_cookie_header(url.host)}"
        f"\r\n"
    )
    s.sendall(req.encode("utf-8"))

    f = s.makefile("rb")
    statusline = f.readline().decode("latin-1")
    if not statusline:
        raise ConnectionError("empty response (stale connection)")
    parts = statusline.split(" ", 2)
    status = int(parts[1]) if len(parts) >= 2 else 0

    headers = {}
    set_cookies = []
    while True:
        line = f.readline().decode("latin-1")
        if line in ("\r\n", "\n", ""):
            break
        if ":" not in line:
            continue
        name, value = line.split(":", 1)
        lname = name.strip().lower()
        # Set-Cookie legitimately repeats; the dict below keeps only the
        # last, so collect them all for the jar separately.
        if lname == "set-cookie":
            set_cookies.append(value.strip())
        headers[lname] = value.strip()
    if set_cookies:
        _store_set_cookie(url.host, set_cookies)

    reusable = "close" not in headers.get("connection", "").casefold() \
        and statusline.startswith("HTTP/1.1")
    if status in (204, 304):
        body = b""
    elif headers.get("transfer-encoding", "").lower() == "chunked":
        body = _read_chunked(f)
    elif "content-length" in headers:
        body = _read_exact(f, int(headers["content-length"]))
    else:
        body = f.read()  # delimited by EOF: connection not reusable
        reusable = False

    if reusable:
        _pool_put(pool_key, s)
    else:
        try:
            s.close()
        except OSError:
            pass
    return status, headers, body


def _read_exact(f, n):
    chunks = []
    remaining = n
    while remaining > 0:
        chunk = f.read(remaining)
        if not chunk:
            break
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def _read_chunked(f):
    out = []
    while True:
        line = f.readline()
        if not line:
            break
        try:
            size = int(line.split(b";")[0].strip() or b"0", 16)
        except ValueError:
            break
        if size == 0:
            while True:  # consume trailers
                t = f.readline()
                if t in (b"\r\n", b"\n", b""):
                    break
            break
        out.append(_read_exact(f, size))
        f.readline()  # trailing CRLF after each chunk
    return b"".join(out)
