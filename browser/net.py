"""Network layer: URL parsing and HTTP/HTTPS fetching.

HTTP/1.1 with persistent connections (per-host socket pool), an
in-memory response cache honoring Cache-Control, TLS, redirects,
chunked transfer encoding, gzip, and file:/about:/data: schemes.
"""

import base64
from dataclasses import dataclass
from email.utils import parsedate_to_datetime
import gzip
import hashlib
import ipaddress
import json
import math
import os
import select
import socket
import ssl
import tempfile
import threading
import time
from typing import Optional
import urllib.parse

from . import psl

# The same string `navigator.userAgent` reports to scripts (ggcore's
# prelude). They have to agree: a server that reads the header and a
# script that reads navigator were describing two different browsers,
# and the bare product token carried no platform, so naver's ad server
# rejected every request with "empty deviceOS". Still identifies as
# GGBrowser -- the Mozilla/AppleWebKit prefix is the compatibility
# boilerplate every engine sends.
USER_AGENT = ("Mozilla/5.0 (Windows NT 10.0; Win64; x64) "
              "AppleWebKit/537.36 (KHTML, like Gecko) "
              "Chrome/122.0.0.0 Safari/537.36 GGBrowser/0.1")
MAX_REDIRECTS = 8
DEFAULT_TIMEOUT = 15.0


class RequestCancelled(ConnectionError):
    """Raised when a caller cancels an in-flight network operation."""


class RequestTimeout(TimeoutError):
    """Raised when the request's total wall-clock deadline expires."""


class CancellationToken:
    """Thread-safe cancellation that interrupts registered sockets."""

    def __init__(self):
        self._event = threading.Event()
        self._lock = threading.Lock()
        self._sockets = set()

    @property
    def cancelled(self):
        return self._event.is_set()

    def check(self):
        if self.cancelled:
            raise RequestCancelled("request cancelled")

    def register(self, sock):
        with self._lock:
            if self._event.is_set():
                try:
                    sock.close()
                except OSError:
                    pass
                raise RequestCancelled("request cancelled")
            self._sockets.add(sock)

    def unregister(self, sock):
        with self._lock:
            self._sockets.discard(sock)

    def cancel(self):
        with self._lock:
            if self._event.is_set():
                return False
            self._event.set()
            sockets = list(self._sockets)
            self._sockets.clear()
        for sock in sockets:
            try:
                sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            try:
                sock.close()
            except OSError:
                pass
        return True


def _request_deadline(timeout):
    if timeout is None:
        return None
    try:
        timeout = float(timeout)
    except (TypeError, ValueError):
        raise ValueError("timeout must be a positive number or None")
    if timeout <= 0 or not math.isfinite(timeout):
        raise ValueError("timeout must be positive")
    return time.monotonic() + timeout


def _remaining_timeout(deadline, cancel_token=None):
    if cancel_token is not None:
        cancel_token.check()
    if deadline is None:
        return None
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise RequestTimeout("request timed out")
    return remaining


def _prepare_socket(sock, deadline, cancel_token=None):
    sock.settimeout(_remaining_timeout(deadline, cancel_token))


class _SocketReader:
    """Small buffered reader that keeps cancellation observable on Windows.

    ``socket.makefile()`` can retain its own socket reference and leave a
    buffered ``read(n)`` blocked after another thread closes the original
    socket. Polling the socket before each recv avoids that platform-specific
    delay while retaining enough buffering for HTTP header lines.
    """

    def __init__(self, sock, deadline, cancel_token):
        self.sock = sock
        self.deadline = deadline
        self.cancel_token = cancel_token
        self.buffer = bytearray()

    def _recv(self, size=65536):
        if self.cancel_token is not None:
            while True:
                self.cancel_token.check()
                pending = 0
                if isinstance(self.sock, ssl.SSLSocket):
                    try:
                        pending = self.sock.pending()
                    except OSError:
                        self.cancel_token.check()
                        raise
                if pending:
                    break
                remaining = _remaining_timeout(
                    self.deadline, self.cancel_token)
                wait = 0.05 if remaining is None else min(remaining, 0.05)
                try:
                    readable, _, _ = select.select(
                        [self.sock], [], [], wait)
                except (OSError, ValueError):
                    self.cancel_token.check()
                    raise
                if readable:
                    break
        _prepare_socket(self.sock, self.deadline, self.cancel_token)
        data = self.sock.recv(max(1, size))
        _remaining_timeout(self.deadline, self.cancel_token)
        return data

    def readline(self):
        while True:
            newline = self.buffer.find(b"\n")
            if newline >= 0:
                end = newline + 1
                line = bytes(self.buffer[:end])
                del self.buffer[:end]
                return line
            chunk = self._recv()
            if not chunk:
                line = bytes(self.buffer)
                self.buffer.clear()
                return line
            self.buffer.extend(chunk)

    def read(self, size=-1):
        if size == 0:
            return b""
        if size is not None and size >= 0:
            if self.buffer:
                take = min(size, len(self.buffer))
                data = bytes(self.buffer[:take])
                del self.buffer[:take]
                return data
            return self._recv(min(size, 65536))
        chunks = []
        if self.buffer:
            chunks.append(bytes(self.buffer))
            self.buffer.clear()
        while True:
            chunk = self._recv()
            if not chunk:
                return b"".join(chunks)
            chunks.append(chunk)

# --- cookie jar ---
# Keyed by (domain, path, name).  The jar deliberately stays in memory, but
# applies the request scoping and security attributes that affect whether a
# cookie may be observed by JS or put on the wire.
@dataclass
class _Cookie:
    name: str
    value: str
    domain: str
    path: str
    host_only: bool
    secure: bool
    http_only: bool
    same_site: str
    expires_at: Optional[float]
    created_at: float


_COOKIE_JAR = {}
_COOKIE_LOCK = threading.RLock()
_COOKIE_NAME_SEPARATORS = set('()<>@,;:\\"/[]?={} \t')
_RESERVED_REQUEST_HEADERS = {
    "host", "connection", "content-length", "cookie", "transfer-encoding",
}
_BODY_HEADERS = {"content-type", "content-encoding"}


def _url_cookie_parts(url_or_host):
    """Return (scheme, normalized host, request path).

    Accepting a host string keeps the small public helpers backwards
    compatible; internal request paths always pass a URL so Secure/Path are
    enforced with the real context.
    """
    if hasattr(url_or_host, "host"):
        scheme = getattr(url_or_host, "scheme", "https").lower()
        host = psl.canonical_host(getattr(url_or_host, "host", ""))
        path = getattr(url_or_host, "path", "/") or "/"
    else:
        scheme = "https"
        host = psl.canonical_host(url_or_host)
        path = "/"
    path = path.split("?", 1)[0]
    if not path.startswith("/"):
        path = "/"
    return scheme, host, path


def _valid_cookie_name(name):
    return bool(name) and all(
        0x21 <= ord(ch) < 0x7f and ch not in _COOKIE_NAME_SEPARATORS
        for ch in name)


def _valid_cookie_value(value):
    # Header injection is the important boundary: reject CR/LF, NUL, TAB,
    # DEL, and every other ASCII control before a value reaches sendall().
    return all(ord(ch) >= 0x20 and ord(ch) != 0x7f for ch in value)


def _domain_match(host, domain):
    return host == domain or host.endswith("." + domain)


def _default_cookie_path(request_path):
    if not request_path.startswith("/") or request_path.count("/") <= 1:
        return "/"
    return request_path.rsplit("/", 1)[0] or "/"


def _path_match(request_path, cookie_path):
    if request_path == cookie_path:
        return True
    if not request_path.startswith(cookie_path):
        return False
    return cookie_path.endswith("/") or request_path[len(cookie_path):].startswith("/")


def _site_key(url_or_host):
    """Return (scheme, registrable domain) per schemeful-site semantics."""
    scheme, host, _path = _url_cookie_parts(url_or_host)
    return scheme, psl.site_domain(host)


def _is_trustworthy_http_host(host):
    if host == "localhost" or host.endswith(".localhost"):
        return True
    try:
        return ipaddress.ip_address(host).is_loopback
    except ValueError:
        return False


def _mixed_content_blocked(site_for_cookies, target,
                           top_level_navigation):
    if top_level_navigation or site_for_cookies is None:
        return False
    source_scheme = getattr(site_for_cookies, "scheme", "")
    return (source_scheme == "https" and target.scheme == "http"
            and not _is_trustworthy_http_host(target.host))


def _same_site_allows(cookie, url, site_for_cookies,
                      top_level_navigation, method):
    if site_for_cookies is None:
        same_site = True
    else:
        same_site = _site_key(url) == _site_key(site_for_cookies)
    if cookie.same_site == "strict":
        return same_site
    if cookie.same_site == "lax":
        return same_site or (
            top_level_navigation and method.upper() in ("GET", "HEAD"))
    return True  # SameSite=None (accepted only together with Secure)


def _purge_expired_locked(now=None):
    now = time.time() if now is None else now
    for key, cookie in list(_COOKIE_JAR.items()):
        if cookie.expires_at is not None and cookie.expires_at <= now:
            del _COOKIE_JAR[key]


def _matching_cookies(url, *, include_http_only, site_for_cookies=None,
                      top_level_navigation=True, method="GET"):
    scheme, host, path = _url_cookie_parts(url)
    with _COOKIE_LOCK:
        _purge_expired_locked()
        found = []
        for cookie in _COOKIE_JAR.values():
            if cookie.host_only:
                if host != cookie.domain:
                    continue
            elif not _domain_match(host, cookie.domain):
                continue
            if not _path_match(path, cookie.path):
                continue
            if cookie.secure and scheme != "https":
                continue
            if cookie.http_only and not include_http_only:
                continue
            if not _same_site_allows(
                    cookie, url, site_for_cookies,
                    top_level_navigation, method):
                continue
            found.append(cookie)
        found.sort(key=lambda c: (-len(c.path), c.created_at))
        return [(c.name, c.value) for c in found]


def _cookie_header(url, *, site_for_cookies=None,
                   top_level_navigation=True, method="GET"):
    pairs = _matching_cookies(
        url, include_http_only=True, site_for_cookies=site_for_cookies,
        top_level_navigation=top_level_navigation, method=method)
    if not pairs:
        return ""
    value = "; ".join(f"{k}={v}" for k, v in pairs)
    return f"Cookie: {value}\r\n"


def _store_set_cookie(url_or_host, values, *, from_js=False):
    """Parse and store Set-Cookie/document.cookie strings.

    Invalid names/values are ignored, matching browser setter behaviour.
    Returns the number of accepted writes/deletions.
    """
    scheme, origin_host, request_path = _url_cookie_parts(url_or_host)
    accepted = 0
    for sc in values:
        if not isinstance(sc, str) or not _valid_cookie_value(sc):
            continue
        parts = [part.strip() for part in sc.split(";")]
        if not parts or "=" not in parts[0]:
            continue
        name, value = parts[0].split("=", 1)
        name, value = name.strip(), value.strip()
        if not _valid_cookie_name(name) or not _valid_cookie_value(value):
            continue

        attrs = {}
        flags = set()
        for part in parts[1:]:
            if not part:
                continue
            if "=" in part:
                attr, attr_value = part.split("=", 1)
                attrs[attr.strip().lower()] = attr_value.strip()
            else:
                flags.add(part.lower())

        host_only = "domain" not in attrs
        domain = origin_host
        if not host_only:
            try:
                domain = psl.canonical_host(
                    attrs["domain"].lstrip("."))
            except ValueError:
                continue
            if not domain or not _domain_match(origin_host, domain):
                continue
            # Public suffixes cannot be used to widen a cookie. If the
            # request host itself is a public suffix, RFC 6265bis treats an
            # identical Domain attribute as absent (host-only).
            if psl.is_public_suffix(domain):
                if domain != origin_host:
                    continue
                host_only = True

        default_path = _default_cookie_path(request_path)
        path = attrs.get("path", default_path)
        if not path.startswith("/"):
            path = default_path

        secure = "secure" in flags
        if secure and scheme != "https":
            continue
        http_only = "httponly" in flags and not from_js
        same_site = attrs.get("samesite", "lax").lower()
        if same_site not in ("strict", "lax", "none"):
            same_site = "lax"
        if same_site == "none" and not secure:
            continue

        if name.startswith("__Secure-") and not (secure and scheme == "https"):
            continue
        if name.startswith("__Host-") and not (
                secure and scheme == "https" and host_only and path == "/"):
            continue

        expires_at = None
        if "max-age" in attrs:
            try:
                max_age = int(attrs["max-age"], 10)
                expires_at = time.time() + max_age
            except ValueError:
                pass
        elif "expires" in attrs:
            try:
                expires_at = parsedate_to_datetime(
                    attrs["expires"]).timestamp()
            except (TypeError, ValueError, OverflowError):
                pass

        key = (domain, path, name)
        with _COOKIE_LOCK:
            _purge_expired_locked()
            old = _COOKIE_JAR.get(key)
            # document.cookie cannot inspect, overwrite, or delete an
            # existing HttpOnly cookie.
            if from_js and old is not None and old.http_only:
                continue
            if expires_at is not None and expires_at <= time.time():
                _COOKIE_JAR.pop(key, None)
                accepted += 1
                continue
            created_at = old.created_at if old is not None else time.monotonic()
            _COOKIE_JAR[key] = _Cookie(
                name=name, value=value, domain=domain, path=path,
                host_only=host_only, secure=secure, http_only=http_only,
                same_site=same_site, expires_at=expires_at,
                created_at=created_at)
            accepted += 1
    return accepted


def cookies_for(url_or_host):
    """The document.cookie-visible string (never includes HttpOnly)."""
    pairs = _matching_cookies(url_or_host, include_http_only=False)
    return "; ".join(f"{k}={v}" for k, v in pairs)


def set_cookie_from_js(url_or_host, cookie_str):
    """Apply a document.cookie write using the page's URL context."""
    return bool(_store_set_cookie(
        url_or_host, [cookie_str], from_js=True))


def _normalize_request(method, body, headers):
    method = str(method or "GET").upper()
    if not _valid_cookie_name(method):
        raise ValueError(f"invalid HTTP method: {method!r}")
    if body is None:
        body = b""
    elif isinstance(body, str):
        body = body.encode("utf-8")
    else:
        body = bytes(body)
    if method in ("GET", "HEAD") and body:
        raise ValueError(f"{method} requests cannot carry a body")

    normalized = {}
    for name, value in (headers or {}).items():
        name = str(name).strip()
        value = str(value).strip()
        if not _valid_cookie_name(name):
            raise ValueError(f"invalid HTTP header name: {name!r}")
        if not _valid_cookie_value(value):
            raise ValueError(f"invalid HTTP header value for {name}")
        lower = name.lower()
        if lower in _RESERVED_REQUEST_HEADERS:
            raise ValueError(f"reserved HTTP header: {name}")
        normalized[lower] = value
    return method, body, normalized

# --- connection pool ---
_POOL = {}
_POOL_LOCK = threading.Lock()
_POOL_MAX_PER_HOST = 6

# --- response cache ---
_CACHE = {}
_CACHE_LOCK = threading.Lock()
_CACHE_MAX_ENTRIES = 300
_CACHE_MAX_VARIANTS = 8
_CACHE_MAX_BODY = 4 * 1024 * 1024
_CACHE_DEFAULT_TTL = 300.0

# --- disk cache (survives restarts) ---
_DISK_DIR = os.environ.get("GG_BROWSER_CACHE_DIR") or os.path.join(
    os.environ.get("LOCALAPPDATA") or tempfile.gettempdir(),
    "gg-browser", "cache")
_DISK_MAX_BODY = 8 * 1024 * 1024
_DISK_MAX_TTL = 7 * 24 * 3600.0


@dataclass
class _CacheEntry:
    headers: dict
    body: bytes
    stored_at: float
    expires_at: float
    vary: tuple
    vary_values: tuple
    revalidate_always: bool = False


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


def _wrap_tls(sock, host, deadline, cancel_token):
    ctx = ssl.create_default_context()
    wrapped = None
    try:
        wrapped = ctx.wrap_socket(
            sock, server_hostname=host, do_handshake_on_connect=False)
        if cancel_token is not None:
            cancel_token.unregister(sock)
            cancel_token.register(wrapped)
        _prepare_socket(wrapped, deadline, cancel_token)
        wrapped.do_handshake()
        return wrapped
    except Exception:
        if wrapped is not None:
            if cancel_token is not None:
                cancel_token.unregister(wrapped)
            try:
                wrapped.close()
            except OSError:
                pass
        raise


def _connect(url, deadline=None, cancel_token=None):
    timeout = _remaining_timeout(deadline, cancel_token)
    proxy = None if _no_proxy(url.host) else _env_proxy(url.scheme)
    if proxy is None:
        # create_connection resolves IPv4 and IPv6 alike
        s = socket.create_connection((url.host, url.port), timeout=timeout)
    else:
        s = socket.create_connection(proxy, timeout=timeout)
    if cancel_token is not None:
        cancel_token.register(s)
    try:
        _prepare_socket(s, deadline, cancel_token)
        if proxy is None:
            if url.scheme == "https":
                s = _wrap_tls(s, url.host, deadline, cancel_token)
            return s

        if url.scheme == "https":
            # CONNECT tunnel, then TLS to the origin through it
            target = f"[{url.host}]" if ":" in url.host else url.host
            _prepare_socket(s, deadline, cancel_token)
            s.sendall((f"CONNECT {target}:{url.port} HTTP/1.1\r\n"
                       f"Host: {target}:{url.port}\r\n\r\n").encode("ascii"))
            head = b""
            while b"\r\n\r\n" not in head:
                _prepare_socket(s, deadline, cancel_token)
                chunk = s.recv(4096)
                if not chunk:
                    raise ConnectionError("proxy closed during CONNECT")
                head += chunk
                if len(head) > 65536:
                    raise ConnectionError("oversized CONNECT response")
            statusline = head.split(b"\r\n", 1)[0].decode("latin-1")
            parts = statusline.split(" ", 2)
            code = int(parts[1]) \
                if len(parts) >= 2 and parts[1].isdigit() else 0
            if code != 200:
                raise ConnectionError(
                    f"proxy CONNECT failed: {statusline}")
            s = _wrap_tls(s, url.host, deadline, cancel_token)
        else:
            # plain http: speak to the proxy directly (absolute-form target)
            s._gg_absolute_form = True
        return s
    except socket.timeout as exc:
        if cancel_token is not None:
            cancel_token.unregister(s)
        try:
            s.close()
        except OSError:
            pass
        raise RequestTimeout("request timed out") from exc
    except Exception:
        if cancel_token is not None:
            cancel_token.unregister(s)
        try:
            s.close()
        except OSError:
            pass
        raise


def _parse_cache_control(value):
    """Parse Cache-Control directives without substring false positives."""
    directives = {}
    current = []
    quoted = False
    escaped = False
    parts = []
    for ch in value or "":
        if escaped:
            current.append(ch)
            escaped = False
        elif quoted and ch == "\\":
            current.append(ch)
            escaped = True
        elif ch == '"':
            current.append(ch)
            quoted = not quoted
        elif ch == "," and not quoted:
            parts.append("".join(current))
            current = []
        else:
            current.append(ch)
    parts.append("".join(current))
    for part in parts:
        name, sep, value = part.partition("=")
        name = name.strip().casefold()
        if not name:
            continue
        value = value.strip()
        if len(value) >= 2 and value[0] == value[-1] == '"':
            value = value[1:-1]
        directives[name] = value if sep else None
    return directives


def _effective_request_headers(request_headers=None):
    headers = {
        "user-agent": USER_AGENT,
        "accept": "text/html,*/*",
        "accept-encoding": "gzip",
    }
    headers.update(request_headers or {})
    return headers


def _vary_fields(headers):
    fields = []
    for item in headers.get("vary", "").split(","):
        name = item.strip().casefold()
        if not name:
            continue
        if name == "*":
            return None
        if name not in fields:
            fields.append(name)
    return tuple(sorted(fields))


def _http_date(value):
    try:
        return parsedate_to_datetime(value).timestamp()
    except (TypeError, ValueError, OverflowError):
        return None


def _cache_entry_from_response(headers, body, request_headers, *, now=None):
    cc = _parse_cache_control(headers.get("cache-control", ""))
    vary = _vary_fields(headers)
    if ("no-store" in cc or vary is None or "set-cookie" in headers
            or len(body) > _DISK_MAX_BODY):
        return None
    now = time.time() if now is None else now
    ttl = _CACHE_DEFAULT_TTL
    if "max-age" in cc:
        try:
            ttl = max(0.0, float(int(cc["max-age"])))
        except (TypeError, ValueError):
            ttl = 0.0
    elif "expires" in headers:
        expires = _http_date(headers.get("expires"))
        date = _http_date(headers.get("date"))
        if expires is not None:
            ttl = max(0.0, expires - (date if date is not None else now))
    try:
        age = max(0.0, float(int(headers.get("age", "0"))))
    except ValueError:
        age = 0.0
    ttl = max(0.0, ttl - age)
    if "no-cache" in cc:
        ttl = 0.0
    effective = _effective_request_headers(request_headers)
    vary_values = tuple((name, effective.get(name, "")) for name in vary)
    return _CacheEntry(
        headers=dict(headers), body=body, stored_at=now,
        expires_at=now + ttl, vary=vary, vary_values=vary_values,
        revalidate_always="no-cache" in cc)


def _cache_matches(entry, request_headers):
    effective = _effective_request_headers(request_headers)
    return all(effective.get(name, "") == value
               for name, value in entry.vary_values)


def _cache_fresh(entry):
    return not entry.revalidate_always and entry.expires_at > time.time()


def _cache_get(key, request_headers=None):
    with _CACHE_LOCK:
        for entry in reversed(_CACHE.get(key, ())):
            if _cache_matches(entry, request_headers):
                return entry
    return None


def _cache_store_memory(key, entry):
    if len(entry.body) > _CACHE_MAX_BODY:
        return
    with _CACHE_LOCK:
        entries = [candidate for candidate in _CACHE.get(key, ())
                   if (candidate.vary, candidate.vary_values)
                   != (entry.vary, entry.vary_values)]
        entries.append(entry)
        _CACHE[key] = entries[-_CACHE_MAX_VARIANTS:]
        if len(_CACHE) > _CACHE_MAX_ENTRIES:
            oldest = min(
                _CACHE,
                key=lambda item: min(e.stored_at for e in _CACHE[item]))
            if oldest != key or len(_CACHE) > 1:
                del _CACHE[oldest]


def _cache_remove(key, request_headers=None):
    with _CACHE_LOCK:
        if request_headers is None:
            _CACHE.pop(key, None)
        else:
            entries = [entry for entry in _CACHE.get(key, ())
                       if not _cache_matches(entry, request_headers)]
            if entries:
                _CACHE[key] = entries
            else:
                _CACHE.pop(key, None)
    path = _disk_path(key)
    disk = _disk_get(key, request_headers, remove_invalid=False)
    if request_headers is None or disk is not None:
        try:
            os.remove(path)
        except OSError:
            pass


def _disk_path(key):
    digest = hashlib.sha256(key.encode("utf-8")).hexdigest()[:32]
    return os.path.join(_DISK_DIR, digest)


def _disk_get(key, request_headers=None, *, remove_invalid=True):
    path = _disk_path(key)
    try:
        with open(path, "rb") as f:
            meta = json.loads(f.readline())
            body = f.read()
        if meta.get("version") == 2:
            entry = _CacheEntry(
                headers=dict(meta["headers"]), body=body,
                stored_at=float(meta["stored_at"]),
                expires_at=float(meta["expires_at"]),
                vary=tuple(meta.get("vary", ())),
                vary_values=tuple(tuple(pair)
                                  for pair in meta.get("vary_values", ())),
                revalidate_always=bool(meta.get("revalidate_always")))
        else:
            # Backward compatibility with the original single-entry format.
            entry = _CacheEntry(
                headers=dict(meta["headers"]), body=body,
                stored_at=float(meta.get("expires", time.time())),
                expires_at=float(meta["expires"]), vary=(), vary_values=())
        if len(body) > _DISK_MAX_BODY:
            raise ValueError("oversized disk cache entry")
        if _cache_matches(entry, request_headers):
            return entry
        return None
    except (OSError, ValueError, TypeError, KeyError, json.JSONDecodeError):
        if remove_invalid:
            try:
                os.remove(path)
            except OSError:
                pass
        return None


def _disk_put(key, entry):
    if len(entry.body) > _DISK_MAX_BODY:
        return
    cc = _parse_cache_control(entry.headers.get("cache-control", ""))
    has_validator = "etag" in entry.headers or "last-modified" in entry.headers
    if ("max-age" not in cc and "expires" not in entry.headers
            and "no-cache" not in cc and not has_validator):
        return
    meta = json.dumps({
        "version": 2,
        "stored_at": entry.stored_at,
        "expires_at": min(
            entry.expires_at, entry.stored_at + _DISK_MAX_TTL),
        "headers": entry.headers,
        "vary": list(entry.vary),
        "vary_values": [list(pair) for pair in entry.vary_values],
        "revalidate_always": entry.revalidate_always,
    })
    try:
        os.makedirs(_DISK_DIR, exist_ok=True)
        fd, tmp = tempfile.mkstemp(dir=_DISK_DIR)
        with os.fdopen(fd, "wb") as f:
            f.write(meta.encode("utf-8"))
            f.write(b"\n")
            f.write(entry.body)
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
            self.host = psl.canonical_host(authority[1:end])
            tail = authority[end + 1:]
            if tail.startswith(":"):
                self.port = int(tail[1:])
        else:
            self.host = authority.lower().rstrip(".")
            if ":" in authority:
                self.host, port = authority.split(":", 1)
                self.port = int(port)
            self.host = psl.canonical_host(self.host)
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


def request_full(url, max_redirects=MAX_REDIRECTS, no_cache=False, *,
                 method="GET", body=None, headers=None,
                 site_for_cookies=None, top_level_navigation=True,
                 send_cookies=True, store_cookies=True,
                 timeout=DEFAULT_TIMEOUT, cancel_token=None):
    """Fetch a URL. Returns (headers: dict, body: bytes, final_url) —
    final_url differs from url after redirects; callers must use it as
    the base for relative links."""
    _status, response_headers, response_body, final_url = \
        request_full_status(
            url, max_redirects, no_cache, method=method, body=body,
            headers=headers, site_for_cookies=site_for_cookies,
            top_level_navigation=top_level_navigation,
            send_cookies=send_cookies, store_cookies=store_cookies,
            timeout=timeout, cancel_token=cancel_token)
    return response_headers, response_body, final_url


def request_full_status(url, max_redirects=MAX_REDIRECTS, no_cache=False, *,
                        method="GET", body=None, headers=None,
                        site_for_cookies=None, top_level_navigation=True,
                        send_cookies=True, store_cookies=True,
                        timeout=DEFAULT_TIMEOUT, cancel_token=None):
    """Fetch a URL and include the HTTP status in the returned tuple."""
    method, body, headers = _normalize_request(method, body, headers)
    deadline = _request_deadline(timeout)
    _remaining_timeout(deadline, cancel_token)
    if _mixed_content_blocked(
            site_for_cookies, url, top_level_navigation):
        raise PermissionError(
            f"blocked mixed content: {site_for_cookies} -> {url}")
    if url.scheme == "about":
        if method not in ("GET", "HEAD"):
            raise ValueError(f"{method} is unsupported for about: URLs")
        from .pages import about_page
        data = about_page(url.path).encode("utf-8")
        return 200, {}, b"" if method == "HEAD" else data, url

    if url.scheme == "data":
        if method not in ("GET", "HEAD"):
            raise ValueError(f"{method} is unsupported for data: URLs")
        headers, body = _data_url(url.path)
        return 200, headers, b"" if method == "HEAD" else body, url

    if url.scheme == "file":
        if method not in ("GET", "HEAD"):
            raise ValueError(f"{method} is unsupported for file: URLs")
        path = url.path
        # a query has no filesystem meaning (form GET submit to file:)
        path = path.split("?", 1)[0].split("#", 1)[0]
        # file:///C:/foo -> C:/foo on Windows
        if len(path) >= 3 and path[0] == "/" and path[2] == ":":
            path = path[1:]
        with open(path, "rb") as f:
            data = f.read()
        _remaining_timeout(deadline, cancel_token)
        return 200, {}, b"" if method == "HEAD" else data, url

    return _http_request(
        url, max_redirects, no_cache,
        method=method, body=body, request_headers=headers,
        site_for_cookies=site_for_cookies,
        top_level_navigation=top_level_navigation,
        send_cookies=send_cookies, store_cookies=store_cookies,
        deadline=deadline, cancel_token=cancel_token)


def request_raw(url, max_redirects=MAX_REDIRECTS, no_cache=False, *,
                method="GET", body=None, headers=None,
                site_for_cookies=None, top_level_navigation=True,
                send_cookies=True, store_cookies=True,
                timeout=DEFAULT_TIMEOUT, cancel_token=None):
    """Fetch a URL. Returns (headers: dict, body: bytes)."""
    headers, body, _ = request_full(
        url, max_redirects, no_cache, method=method, body=body,
        headers=headers, site_for_cookies=site_for_cookies,
        top_level_navigation=top_level_navigation,
        send_cookies=send_cookies, store_cookies=store_cookies,
        timeout=timeout, cancel_token=cancel_token)
    return headers, body


def request_text(url, max_redirects=MAX_REDIRECTS, no_cache=False, *,
                 method="GET", body=None, headers=None,
                 site_for_cookies=None, top_level_navigation=True,
                 send_cookies=True, store_cookies=True,
                 timeout=DEFAULT_TIMEOUT, cancel_token=None):
    """Fetch and decode as text. Returns (headers, str, final_url)."""
    headers, body, final_url = request_full(
        url, max_redirects, no_cache, method=method, body=body,
        headers=headers,
        site_for_cookies=site_for_cookies,
        top_level_navigation=top_level_navigation,
        send_cookies=send_cookies, store_cookies=store_cookies,
        timeout=timeout, cancel_token=cancel_token)
    charset = "utf-8"
    ctype = headers.get("content-type", "")
    if "charset=" in ctype:
        charset = ctype.split("charset=", 1)[1].split(";")[0].strip().strip('"')
    try:
        return headers, body.decode(charset, errors="replace"), final_url
    except LookupError:
        return headers, body.decode("utf-8", errors="replace"), final_url


def request(url, max_redirects=MAX_REDIRECTS, *, method="GET", body=None,
            headers=None, site_for_cookies=None, top_level_navigation=True,
            timeout=DEFAULT_TIMEOUT, cancel_token=None):
    """Fetch a URL and decode it as text. Returns (headers, str)."""
    headers, text, _ = request_text(
        url, max_redirects, method=method, body=body, headers=headers,
        site_for_cookies=site_for_cookies,
        top_level_navigation=top_level_navigation,
        timeout=timeout, cancel_token=cancel_token)
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


def _http_request(url, redirects_left, no_cache=False, *, method="GET",
                  body=b"", request_headers=None, site_for_cookies=None,
                  top_level_navigation=True, send_cookies=True,
                  store_cookies=True, deadline=None, cancel_token=None):
    _remaining_timeout(deadline, cancel_token)
    request_body = body
    original_request_headers = dict(request_headers or {})
    # fragments never reach the wire: exclude them from the cache key
    cache_key = f"{url.scheme}://{url.host}:{url.port}{url.path}"
    cookie_header = (_cookie_header(
        url, site_for_cookies=site_for_cookies,
        top_level_navigation=top_level_navigation, method=method)
        if send_cookies else "")
    effective_headers = _effective_request_headers(original_request_headers)
    request_cc = _parse_cache_control(
        effective_headers.get("cache-control", ""))
    cache_allowed = (
        method == "GET" and not cookie_header
        and "authorization" not in effective_headers
        and "proxy-authorization" not in effective_headers
        and not any(name in effective_headers for name in {
            "range", "if-range", "if-match", "if-none-match",
            "if-modified-since", "if-unmodified-since"}))
    cache_read_allowed = cache_allowed and "no-store" not in request_cc
    cache_store_allowed = cache_allowed and "no-store" not in request_cc
    force_validation = (
        no_cache or "no-cache" in request_cc
        or effective_headers.get("pragma", "").casefold() == "no-cache")
    if "max-age" in request_cc:
        try:
            force_validation = force_validation or int(
                request_cc["max-age"]) <= 0
        except (TypeError, ValueError):
            pass

    cached_entry = None
    revalidating_entry = None
    if cache_read_allowed:
        cached_entry = _cache_get(cache_key, original_request_headers)
        if cached_entry is None:
            cached_entry = _disk_get(cache_key, original_request_headers)
            if cached_entry is not None:
                _cache_store_memory(cache_key, cached_entry)
        if (cached_entry is not None and not force_validation
                and _cache_fresh(cached_entry)):
            return 200, dict(cached_entry.headers), cached_entry.body, url
        if cached_entry is not None:
            wire_headers = dict(original_request_headers)
            if ("if-none-match" not in wire_headers
                    and "etag" in cached_entry.headers):
                wire_headers["if-none-match"] = cached_entry.headers["etag"]
                revalidating_entry = cached_entry
            elif ("if-modified-since" not in wire_headers
                  and "last-modified" in cached_entry.headers):
                wire_headers["if-modified-since"] = \
                    cached_entry.headers["last-modified"]
                revalidating_entry = cached_entry
            request_headers = wire_headers

    pool_key = (url.scheme, url.host, url.port)
    last_error = None
    for attempt in range(3):
        s = _pool_get(pool_key)
        reused = s is not None
        try:
            if s is None:
                s = _connect(url, deadline, cancel_token)
            elif cancel_token is not None:
                cancel_token.register(s)
            result = _one_request(
                url, s, pool_key, cookie_header, method=method,
                body=request_body,
                request_headers=request_headers,
                store_cookies=store_cookies,
                deadline=deadline, cancel_token=cancel_token)
            break
        except RequestCancelled:
            if cancel_token is not None and s is not None:
                cancel_token.unregister(s)
            try:
                if s is not None:
                    s.close()
            except OSError:
                pass
            raise
        except (RequestTimeout, socket.timeout) as e:
            if cancel_token is not None and s is not None:
                cancel_token.unregister(s)
            try:
                if s is not None:
                    s.close()
            except OSError:
                pass
            raise RequestTimeout("request timed out") from e
        except (OSError, ssl.SSLError, ConnectionError) as e:
            if cancel_token is not None and s is not None:
                cancel_token.unregister(s)
            try:
                if s is not None:
                    s.close()
            except OSError:
                pass
            _remaining_timeout(deadline, cancel_token)
            last_error = e
            if not reused:  # fresh connection genuinely failed
                raise
    else:
        raise last_error

    status, headers, body = result

    if status == 304 and revalidating_entry is not None:
        # A successful conditional request refreshes metadata while reusing
        # the selected representation's body. Hop-by-hop/framing fields from
        # a 304 do not describe the returned cached payload.
        merged = dict(revalidating_entry.headers)
        for name, value in headers.items():
            if name not in {
                    "connection", "content-length", "keep-alive",
                    "proxy-authenticate", "proxy-authorization", "te",
                    "trailer", "transfer-encoding", "upgrade"}:
                merged[name] = value
        refreshed = _cache_entry_from_response(
            merged, revalidating_entry.body, original_request_headers)
        if cache_store_allowed and refreshed is not None:
            _cache_remove(cache_key, original_request_headers)
            _cache_store_memory(cache_key, refreshed)
            _disk_put(cache_key, refreshed)
        else:
            _cache_remove(cache_key, original_request_headers)
        return 200, merged, revalidating_entry.body, url

    if revalidating_entry is not None and status != 304:
        _cache_remove(cache_key, original_request_headers)

    if 300 <= status < 400 and "location" in headers:
        if redirects_left <= 0:
            raise RuntimeError("too many redirects")
        target = url.resolve(headers["location"])
        if _mixed_content_blocked(
                site_for_cookies, target, top_level_navigation):
            raise PermissionError(
                f"blocked mixed-content redirect: {url} -> {target}")
        next_method, next_body = method, request_body
        # Cache-generated conditional headers select only this URL and must
        # not leak onto a redirect target.
        next_headers = dict(original_request_headers)
        if ((status in (301, 302) and method == "POST")
                or (status == 303 and method != "HEAD")):
            next_method, next_body = "GET", b""
            for name in _BODY_HEADERS:
                next_headers.pop(name, None)
        if (target.scheme, target.host, target.port) != (
                url.scheme, url.host, url.port):
            next_headers.pop("authorization", None)
            next_headers.pop("proxy-authorization", None)
        return _http_request(
            target, redirects_left - 1, no_cache,
            method=next_method, body=next_body,
            request_headers=next_headers,
            site_for_cookies=site_for_cookies,
            top_level_navigation=top_level_navigation,
            send_cookies=send_cookies, store_cookies=store_cookies,
            deadline=deadline, cancel_token=cancel_token)

    if headers.get("content-encoding", "").lower() == "gzip":
        body = gzip.decompress(body) if body else b""
        headers = dict(headers)
        del headers["content-encoding"]  # body is stored decoded
    if status == 200 and cache_store_allowed:
        entry = _cache_entry_from_response(
            headers, body, original_request_headers)
        if entry is not None:
            if cached_entry is not None:
                _cache_remove(cache_key, original_request_headers)
            _cache_store_memory(cache_key, entry)
            _disk_put(cache_key, entry)
        else:
            _cache_remove(cache_key, original_request_headers)
    elif cache_allowed and "no-store" in request_cc:
        _cache_remove(cache_key, original_request_headers)
    return status, headers, body, url


def _safe_path(path):
    """Request-target with control chars/spaces percent-encoded, so a
    crafted URL can never inject headers into the request."""
    return urllib.parse.quote(path, safe="/?#[]@!$&'()*+,;=:%~._-")


def _one_request(url, s, pool_key, cookie_header="", *, method="GET",
                 body=b"", request_headers=None, store_cookies=True,
                 deadline=None, cancel_token=None):
    """Send one request on an open socket; returns (status, headers, body).
    Returns the socket to the pool when the response allows reuse."""
    host = f"[{url.host}]" if ":" in url.host else url.host
    default = 80 if url.scheme == "http" else 443
    if url.port != default:
        host += f":{url.port}"  # RFC 7230: Host carries non-default port
    target = _safe_path(url.path)
    if getattr(s, "_gg_absolute_form", False):
        # plain-http via an environment proxy: absolute-form target
        target = f"{url.scheme}://{host}{target}"
    lines = [
        f"{method} {target} HTTP/1.1",
        f"Host: {host}",
        "Connection: keep-alive",
    ]
    for name, value in _effective_request_headers(request_headers).items():
        lines.append(f"{name}: {value}")
    if body or method in ("POST", "PUT", "PATCH"):
        lines.append(f"Content-Length: {len(body)}")
    if cookie_header:
        lines.append(cookie_header.removesuffix("\r\n"))
    req = "\r\n".join(lines) + "\r\n\r\n"
    _prepare_socket(s, deadline, cancel_token)
    s.sendall(req.encode("utf-8") + body)

    f = _SocketReader(s, deadline, cancel_token)
    statusline = _readline(
        f, s, deadline, cancel_token).decode("latin-1")
    if not statusline:
        raise ConnectionError("empty response (stale connection)")
    parts = statusline.split(" ", 2)
    status = int(parts[1]) if len(parts) >= 2 else 0

    headers = {}
    set_cookies = []
    while True:
        line = _readline(
            f, s, deadline, cancel_token).decode("latin-1")
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
    if set_cookies and store_cookies:
        _store_set_cookie(url, set_cookies)

    reusable = "close" not in headers.get("connection", "").casefold() \
        and statusline.startswith("HTTP/1.1")
    if method == "HEAD" or status in (204, 304):
        body = b""
    elif headers.get("transfer-encoding", "").lower() == "chunked":
        body = _read_chunked(f, s, deadline, cancel_token)
    elif "content-length" in headers:
        body = _read_exact(
            f, int(headers["content-length"]), s, deadline, cancel_token)
    else:
        _prepare_socket(s, deadline, cancel_token)
        body = f.read()  # delimited by EOF: connection not reusable
        _remaining_timeout(deadline, cancel_token)
        reusable = False

    if reusable:
        _remaining_timeout(deadline, cancel_token)
        if cancel_token is not None:
            cancel_token.unregister(s)
        _pool_put(pool_key, s)
    else:
        if cancel_token is not None:
            cancel_token.unregister(s)
        try:
            s.close()
        except OSError:
            pass
    return status, headers, body


def _readline(f, sock, deadline, cancel_token):
    _prepare_socket(sock, deadline, cancel_token)
    line = f.readline()
    _remaining_timeout(deadline, cancel_token)
    return line


def _read_exact(f, n, sock=None, deadline=None, cancel_token=None):
    chunks = []
    remaining = n
    while remaining > 0:
        if sock is not None:
            _prepare_socket(sock, deadline, cancel_token)
        chunk = f.read(remaining)
        _remaining_timeout(deadline, cancel_token)
        if not chunk:
            break
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def _read_chunked(f, sock=None, deadline=None, cancel_token=None):
    out = []
    while True:
        line = (_readline(f, sock, deadline, cancel_token)
                if sock is not None else f.readline())
        if not line:
            break
        try:
            size = int(line.split(b";")[0].strip() or b"0", 16)
        except ValueError:
            break
        if size == 0:
            while True:  # consume trailers
                t = (_readline(f, sock, deadline, cancel_token)
                     if sock is not None else f.readline())
                if t in (b"\r\n", b"\n", b""):
                    break
            break
        out.append(_read_exact(
            f, size, sock, deadline, cancel_token))
        if sock is not None:
            _readline(f, sock, deadline, cancel_token)
        else:
            f.readline()  # trailing CRLF after each chunk
    return b"".join(out)
