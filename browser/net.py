"""Network layer: URL parsing and HTTP/HTTPS fetching.

HTTP/1.1 with persistent connections (per-host socket pool), an
in-memory response cache honoring Cache-Control, TLS, redirects,
chunked transfer encoding, gzip, and file:/about:/data: schemes.
"""

import base64
import gzip
import hashlib
import json
import re
import os
import socket
import ssl
import tempfile
import threading
import time
import urllib.parse

USER_AGENT = "GGBrowser/0.1 (educational engine)"
MAX_REDIRECTS = 8

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

# --- cookie jar v2 (T3: attribute-complete, in-memory) ---
# (domain, path, name) -> record. Spec-shaped semantics: Domain
# suffix matching, Path boundaries, Expires/Max-Age, Secure,
# HttpOnly (hidden from document.cookie), SameSite (Strict enforced
# against a cross-site initiator; Lax approximated as send — our
# subresource fetches act on the page's behalf). No public-suffix
# list yet: same-site compares the last two labels.
_COOKIES = {}
_COOKIE_LOCK = threading.Lock()


def _site_of(host):
    parts = host.lower().rstrip(".").split(".")
    return ".".join(parts[-2:]) if len(parts) >= 2 else host.lower()


def same_site(a, b):
    return _site_of(a) == _site_of(b)


def _cookie_expired(rec, now=None):
    exp = rec.get("expires")
    return exp is not None and exp <= (now or time.time())


def store_cookie(host, line, scheme="https"):
    """Record one Set-Cookie line with its attributes."""
    host = host.lower()
    parts = [p.strip() for p in line.split(";")]
    if not parts or "=" not in parts[0]:
        return
    name, value = parts[0].split("=", 1)
    name = name.strip()
    if not name:
        return
    attrs = {}
    for a in parts[1:]:
        k, _, v = a.partition("=")
        attrs[k.strip().lower()] = v.strip()
    if "secure" in attrs and scheme != "https":
        return
    domain = attrs.get("domain", "").lstrip(".").lower()
    host_only = not domain
    if domain and host != domain \
            and not host.endswith("." + domain):
        return  # a host may not set cookies for unrelated domains
    if host_only:
        domain = host
    path = attrs.get("path", "")
    if not path.startswith("/"):
        path = "/"
    expires = None
    if "max-age" in attrs:
        try:
            expires = time.time() + float(attrs["max-age"])
        except ValueError:
            pass
    elif "expires" in attrs:
        try:
            from email.utils import parsedate_to_datetime
            expires = parsedate_to_datetime(
                attrs["expires"]).timestamp()
        except (ValueError, TypeError):
            pass
    key = (domain, path, name)
    with _COOKIE_LOCK:
        if expires is not None and expires <= time.time():
            _COOKIES.pop(key, None)
            return
        _COOKIES[key] = {
            "value": value.strip(),
            "secure": "secure" in attrs,
            "httponly": "httponly" in attrs,
            "samesite": attrs.get("samesite", "lax").lower() or "lax",
            "expires": expires,
            "host_only": host_only,
        }


def _cookie_matches(key, rec, host, scheme, path, initiator):
    domain, cpath, _name = key
    if rec["host_only"]:
        if host != domain:
            return False
    elif host != domain and not host.endswith("." + domain):
        return False
    if not (path == cpath or cpath == "/"
            or path.startswith(cpath.rstrip("/") + "/")):
        return False
    if rec["secure"] and scheme != "https":
        return False
    if rec["samesite"] == "strict" and initiator \
            and not same_site(initiator, host):
        return False
    return not _cookie_expired(rec)


def cookie_header(host, scheme="https", path="/", initiator=None):
    """The Cookie: header value for this request ('' if none).
    Longer paths first, per spec."""
    host = host.lower()
    path = path.split("?", 1)[0] or "/"
    out = []
    with _COOKIE_LOCK:
        dead = [k for k, r in _COOKIES.items() if _cookie_expired(r)]
        for k in dead:
            del _COOKIES[k]
        for key, rec in _COOKIES.items():
            if _cookie_matches(key, rec, host, scheme, path, initiator):
                out.append((key[1], key[2], rec["value"]))
    out.sort(key=lambda t: -len(t[0]))
    return "; ".join(f"{n}={v}" for (_p, n, v) in out)


def cookies_for(host):
    """name->value for document.cookie (HttpOnly excluded)."""
    host = host.lower()
    with _COOKIE_LOCK:
        return {k[2]: r["value"] for k, r in _COOKIES.items()
                if not r["httponly"] and not _cookie_expired(r)
                and _cookie_matches(k, r, host, "https", "/", None)}


def seed_cookies(host, pairs):
    """JS document.cookie writes flowing back: host-only, path=/."""
    host = host.lower()
    with _COOKIE_LOCK:
        for name, value in dict(pairs).items():
            _COOKIES[(host, "/", name)] = {
                "value": value, "secure": False, "httponly": False,
                "samesite": "lax", "expires": None, "host_only": True,
            }


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


def _connect(url):
    # create_connection resolves IPv4 and IPv6 alike
    s = socket.create_connection((url.host, url.port), timeout=15)
    if url.scheme == "https":
        ctx = ssl.create_default_context()
        s = ctx.wrap_socket(s, server_hostname=url.host)
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


def request_full(url, max_redirects=MAX_REDIRECTS, no_cache=False,
                 method="GET", body=None, content_type=None,
                 initiator=None):
    """Fetch a URL. Returns (headers: dict, body: bytes, final_url) —
    final_url differs from url after redirects; callers must use it as
    the base for relative links. initiator (a host) is the requesting
    page's host for SameSite cookie decisions."""
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

    return _http_request(url, max_redirects, no_cache, method,
                         body, content_type, initiator)


def request_raw(url, max_redirects=MAX_REDIRECTS):
    """Fetch a URL. Returns (headers: dict, body: bytes)."""
    headers, body, _ = request_full(url, max_redirects)
    return headers, body


_META_CHARSET_RE = re.compile(
    rb"""<meta[^>]+charset\s*=\s*["']?([a-zA-Z0-9_\-]+)""", re.I)


def sniff_charset(body, ctype=""):
    """The document's charset: Content-Type header first, then a
    <meta charset=...> / http-equiv sniff of the head (legacy Korean
    sites are EUC-KR-with-silent-headers), utf-8 otherwise."""
    if "charset=" in ctype:
        return (ctype.split("charset=", 1)[1].split(";")[0]
                .strip().strip('"'))
    m = _META_CHARSET_RE.search(body[:2048])
    if m:
        return m.group(1).decode("ascii", errors="replace")
    return "utf-8"


def request_text(url, max_redirects=MAX_REDIRECTS, no_cache=False,
                 method="GET", body=None, content_type=None,
                 initiator=None):
    """Fetch and decode as text. Returns (headers, str, final_url)."""
    headers, body, final_url = request_full(
        url, max_redirects, no_cache, method, body, content_type,
        initiator)
    charset = sniff_charset(body, headers.get("content-type", ""))
    try:
        return headers, body.decode(charset, errors="replace"), final_url
    except LookupError:
        return headers, body.decode("utf-8", errors="replace"), final_url


class CorsError(Exception):
    """A cross-origin fetch()/XHR response did not opt in via CORS."""


def origin_of(url):
    """The serialized origin of a URL ('null' for opaque schemes)."""
    if url.scheme not in ("http", "https"):
        return "null"
    default = 80 if url.scheme == "http" else 443
    port = "" if url.port == default else f":{url.port}"
    return f"{url.scheme}://{url.host}{port}"


def same_origin(a, b):
    return (a.scheme, a.host, a.port) == (b.scheme, b.host, b.port)


def cors_allows(page_url, target_url, resp_headers):
    """The CORS response check for a simple cross-origin request:
    Access-Control-Allow-Origin must be * or the page's origin.
    Same-origin (and non-HTTP schemes, e.g. file: pages reading
    file: fixtures) always pass."""
    if same_origin(page_url, target_url):
        return True
    if target_url.scheme not in ("http", "https"):
        return page_url.scheme == target_url.scheme
    acao = resp_headers.get(
        "access-control-allow-origin", "").strip()
    return acao == "*" or (acao != "" and acao == origin_of(page_url))


def fetch_for_page(page_url, target_url):
    """A fetch()/XHR load on behalf of page_url: carries the page's
    host as the SameSite initiator and enforces the CORS response
    check (the fetch channel only issues simple GETs, so there is no
    preflight). Embedded resources (script/img/link) are no-cors and
    do NOT go through here. Returns (headers, text, final_url)."""
    headers, text, final = request_text(
        target_url, no_cache=True,
        initiator=page_url.host or None)
    if not cors_allows(page_url, final, headers):
        raise CorsError(
            f"CORS: {origin_of(final)} did not allow "
            f"{origin_of(page_url)}")
    return headers, text, final


def request_post_text(url, data, initiator=None):
    """POST an urlencoded form. Returns (headers, str, final_url)."""
    if isinstance(data, dict):
        data = urllib.parse.urlencode(data)
    if isinstance(data, str):
        data = data.encode("utf-8")
    return request_text(
        url, no_cache=True, method="POST", body=data,
        content_type="application/x-www-form-urlencoded",
        initiator=initiator)


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


def _http_request(url, redirects_left, no_cache=False, method="GET",
                  body=None, content_type=None, initiator=None):
    # fragments never reach the wire: exclude them from the cache key
    cache_key = f"{url.scheme}://{url.host}:{url.port}{url.path}"
    if method != "GET":
        no_cache = True
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
            result = _one_request(url, s, pool_key, method,
                                  body, content_type, initiator)
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
        # browsers turn a redirected POST into a GET (303 always;
        # 301/302 in practice)
        if method != "GET" and status in (301, 302, 303):
            method, body, content_type = "GET", None, None
        return _http_request(target, redirects_left - 1, no_cache,
                             method, body, content_type, initiator)

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


def _one_request(url, s, pool_key, method="GET", body=None,
                 content_type=None, initiator=None):
    """Send one request on an open socket; returns (status, headers,
    body). Returns the socket to the pool when the response allows
    reuse."""
    host = f"[{url.host}]" if ":" in url.host else url.host
    default = 80 if url.scheme == "http" else 443
    if url.port != default:
        host += f":{url.port}"  # RFC 7230: Host carries non-default port
    cookies = cookie_header(url.host, url.scheme, url.path, initiator)
    cookie_line = f"Cookie: {cookies}\r\n" if cookies else ""
    extra = ""
    payload = b""
    if body is not None:
        payload = body if isinstance(body, bytes) \
            else str(body).encode("utf-8")
        ct = content_type or "application/x-www-form-urlencoded"
        extra = (f"Content-Type: {ct}\r\n"
                 f"Content-Length: {len(payload)}\r\n")
    req = (
        f"{method} {_safe_path(url.path)} HTTP/1.1\r\n"
        f"Host: {host}\r\n"
        f"Connection: keep-alive\r\n"
        f"User-Agent: {USER_AGENT}\r\n"
        f"Accept: text/html,*/*\r\n"
        f"Accept-Encoding: gzip\r\n"
        f"{cookie_line}"
        f"{extra}"
        f"\r\n"
    )
    s.sendall(req.encode("utf-8") + payload)

    f = s.makefile("rb")
    statusline = f.readline().decode("latin-1")
    if not statusline:
        raise ConnectionError("empty response (stale connection)")
    parts = statusline.split(" ", 2)
    status = int(parts[1]) if len(parts) >= 2 else 0

    headers = {}
    while True:
        line = f.readline().decode("latin-1")
        if line in ("\r\n", "\n", ""):
            break
        if ":" not in line:
            continue
        name, value = line.split(":", 1)
        lname = name.strip().lower()
        if lname == "set-cookie":
            store_cookie(url.host, value.strip(), url.scheme)
        headers[lname] = value.strip()

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
