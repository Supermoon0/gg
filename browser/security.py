"""Origin, CORS, credentials, and script-fetch security policy."""

from dataclasses import dataclass

from . import net


class FetchPolicyError(RuntimeError):
    pass


@dataclass(frozen=True)
class ScriptFetchResponse:
    status: int
    headers: dict
    body: str
    final_url: object
    opaque: bool = False


_SAFE_METHODS = {"GET", "HEAD", "POST"}
_SAFE_HEADERS = {"accept", "accept-language", "content-language"}
_SAFE_CONTENT_TYPES = {
    "application/x-www-form-urlencoded",
    "multipart/form-data",
    "text/plain",
}
_FORBIDDEN_METHODS = {"CONNECT", "TRACE", "TRACK"}


def origin_tuple(url):
    if getattr(url, "scheme", "") not in ("http", "https"):
        return None
    return url.scheme.lower(), url.host.lower().rstrip("."), int(url.port)


def serialize_origin(url):
    origin = origin_tuple(url)
    if origin is None:
        return "null"
    scheme, host, port = origin
    default = 80 if scheme == "http" else 443
    shown_host = f"[{host}]" if ":" in host else host
    suffix = "" if port == default else f":{port}"
    return f"{scheme}://{shown_host}{suffix}"


def same_origin(left, right):
    left_origin = origin_tuple(left)
    right_origin = origin_tuple(right)
    if left_origin is None or right_origin is None:
        return str(left) == str(right)
    return left_origin == right_origin


def _normalized_headers(pairs):
    headers = {}
    for name, value in pairs or ():
        headers[str(name).strip().lower()] = str(value).strip()
    return headers


def _is_safelisted_header(name, value):
    if name in _SAFE_HEADERS:
        return True
    if name != "content-type":
        return False
    media_type = value.split(";", 1)[0].strip().lower()
    return media_type in _SAFE_CONTENT_TYPES


def _unsafe_header_names(headers):
    return sorted(name for name, value in headers.items()
                  if not _is_safelisted_header(name, value))


def _cors_allows(response_headers, request_origin, credentials_included):
    allowed = response_headers.get("access-control-allow-origin", "").strip()
    if allowed == "*":
        if credentials_included:
            raise FetchPolicyError(
                "CORS wildcard is invalid with credentials")
    elif allowed != request_origin:
        raise FetchPolicyError(
            f"CORS origin denied: expected {request_origin!r}, got {allowed!r}")
    if credentials_included and response_headers.get(
            "access-control-allow-credentials", "").lower() != "true":
        raise FetchPolicyError("CORS credentials were not allowed")


def _preflight(base_url, target, method, headers, request_origin,
               credentials_included, *, timeout=net.DEFAULT_TIMEOUT,
               cancel_token=None):
    unsafe = _unsafe_header_names(headers)
    preflight_headers = {
        "Origin": request_origin,
        "Access-Control-Request-Method": method,
    }
    if unsafe:
        preflight_headers["Access-Control-Request-Headers"] = ", ".join(unsafe)
    status, response_headers, _body, _final = net.request_full_status(
        target, no_cache=True, method="OPTIONS", headers=preflight_headers,
        site_for_cookies=base_url, top_level_navigation=False,
        send_cookies=False, store_cookies=False, timeout=timeout,
        cancel_token=cancel_token)
    if not 200 <= status < 300:
        raise FetchPolicyError(f"CORS preflight failed with HTTP {status}")
    _cors_allows(response_headers, request_origin, credentials_included)
    allowed_methods = {
        item.strip().upper() for item in response_headers.get(
            "access-control-allow-methods", "").split(",") if item.strip()
    }
    if method not in allowed_methods:
        raise FetchPolicyError(f"CORS method {method} was not allowed")
    if unsafe:
        raw_allowed = response_headers.get(
            "access-control-allow-headers", "")
        allowed_headers = {
            item.strip().lower() for item in raw_allowed.split(",")
            if item.strip()
        }
        if not ("*" in allowed_headers and not credentials_included) \
                and not set(unsafe).issubset(allowed_headers):
            raise FetchPolicyError(
                f"CORS headers were not allowed: {unsafe!r}")


def perform_script_fetch(base_url, request, *, timeout=net.DEFAULT_TIMEOUT,
                         cancel_token=None):
    """Perform one fetch/XHR request under browser security rules.

    request is a dict produced by native.script_fetches(): url, method,
    body, headers, mode, and credentials.
    """
    target = base_url.resolve(request.get("url", ""))
    method = str(request.get("method") or "GET").upper()
    if method in _FORBIDDEN_METHODS:
        raise FetchPolicyError(f"forbidden fetch method: {method}")
    mode = str(request.get("mode") or "cors").lower()
    if mode not in ("cors", "same-origin", "no-cors"):
        raise FetchPolicyError(f"unsupported fetch mode: {mode}")
    credentials = str(
        request.get("credentials") or "same-origin").lower()
    if credentials not in ("omit", "same-origin", "include"):
        raise FetchPolicyError(
            f"unsupported credentials mode: {credentials}")

    cross_origin = not same_origin(base_url, target)
    if mode == "same-origin" and cross_origin:
        raise FetchPolicyError("cross-origin request blocked by same-origin mode")
    headers = _normalized_headers(request.get("headers"))
    body = request.get("body") or ""
    credentials_included = credentials == "include" or (
        credentials == "same-origin" and not cross_origin)

    if mode == "no-cors":
        if method not in _SAFE_METHODS or _unsafe_header_names(headers):
            raise FetchPolicyError(
                "no-cors only permits CORS-safelisted methods and headers")
    else:
        # Supplying Origin on same-origin requests also keeps it correct if
        # the request redirects across origins inside net.py.
        headers["origin"] = serialize_origin(base_url)
        if cross_origin and (
                method not in _SAFE_METHODS or _unsafe_header_names(
                    {k: v for k, v in headers.items() if k != "origin"})):
            _preflight(
                base_url, target, method,
                {k: v for k, v in headers.items() if k != "origin"},
                serialize_origin(base_url), credentials_included,
                timeout=timeout, cancel_token=cancel_token)

    status, response_headers, response_body, final_url = \
        net.request_full_status(
            target, no_cache=True, method=method,
            body=body.encode("utf-8") if isinstance(body, str) else body,
            headers=headers, site_for_cookies=base_url,
            top_level_navigation=False,
            send_cookies=credentials_included,
            store_cookies=credentials_included, timeout=timeout,
            cancel_token=cancel_token)
    final_cross_origin = not same_origin(base_url, final_url)
    if mode == "cors" and final_cross_origin:
        _cors_allows(
            response_headers, serialize_origin(base_url),
            credentials_included)
    if mode == "no-cors" and final_cross_origin:
        return ScriptFetchResponse(
            0, {}, "", final_url, opaque=True)
    charset = "utf-8"
    content_type = response_headers.get("content-type", "")
    if "charset=" in content_type:
        charset = content_type.split("charset=", 1)[1] \
            .split(";", 1)[0].strip().strip('"')
    try:
        text = response_body.decode(charset, errors="replace")
    except LookupError:
        text = response_body.decode("utf-8", errors="replace")
    return ScriptFetchResponse(
        status, response_headers, text, final_url)
