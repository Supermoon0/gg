"""Public Suffix List lookup and schemeful-site domain calculation."""

from functools import lru_cache
import ipaddress
from pathlib import Path
import threading


_PSL_PATH = Path(__file__).with_name("data") / "public_suffix_list.dat"
_RULE_LOCK = threading.Lock()
_RULES = None

# Secure, deliberately small fallback for source trees/packages that omitted
# the vendored data. The updater and release validation require the full list.
_FALLBACK_EXACT = {
    "com", "edu", "gov", "io", "jp", "net", "org", "uk", "co.uk",
    "com.au", "co.jp", "github.io", "appspot.com",
}
_FALLBACK_WILDCARDS = {"ck", "kawasaki.jp"}
_FALLBACK_EXCEPTIONS = {"www.ck", "city.kawasaki.jp"}


def canonical_host(host):
    """Lower-case and IDNA-canonicalize a DNS name or IP literal."""
    host = str(host or "").strip().rstrip(".").lower()
    host = host.replace("\u3002", ".").replace("\uff0e", ".") \
        .replace("\uff61", ".")
    if not host:
        return ""
    try:
        return ipaddress.ip_address(host).compressed.lower()
    except ValueError:
        pass
    labels = host.split(".")
    if any(not label for label in labels):
        raise ValueError(f"invalid host: {host!r}")
    try:
        return ".".join(
            label.encode("idna").decode("ascii").lower()
            for label in labels)
    except UnicodeError as exc:
        raise ValueError(f"invalid IDNA host: {host!r}") from exc


def _canonical_rule(rule):
    return canonical_host(rule)


def _load_rules():
    global _RULES
    if _RULES is not None:
        return _RULES
    with _RULE_LOCK:
        if _RULES is not None:
            return _RULES
        exact = set()
        wildcards = set()
        exceptions = set()
        version = "fallback"
        try:
            with _PSL_PATH.open(encoding="utf-8") as source:
                for raw_line in source:
                    line = raw_line.strip()
                    if line.startswith("// VERSION:"):
                        version = line.removeprefix("// VERSION:").strip()
                    if not line or line.startswith("//"):
                        continue
                    rule = line.split(None, 1)[0]
                    if rule.startswith("!"):
                        exceptions.add(_canonical_rule(rule[1:]))
                    elif rule.startswith("*."):
                        wildcards.add(_canonical_rule(rule[2:]))
                    else:
                        exact.add(_canonical_rule(rule))
            if not exact or "com" not in exact or "co.uk" not in exact:
                raise ValueError("incomplete PSL data")
        except (OSError, UnicodeError, ValueError):
            exact = set(_FALLBACK_EXACT)
            wildcards = set(_FALLBACK_WILDCARDS)
            exceptions = set(_FALLBACK_EXCEPTIONS)
            version = "fallback"
        _RULES = (
            frozenset(exact), frozenset(wildcards),
            frozenset(exceptions), version)
        return _RULES


def version():
    return _load_rules()[3]


@lru_cache(maxsize=4096)
def public_suffix(host):
    """Return the canonical public suffix, applying exact/*/! rules."""
    host = canonical_host(host)
    if not host:
        return ""
    try:
        ipaddress.ip_address(host)
        return host
    except ValueError:
        pass
    labels = host.split(".")
    exact, wildcards, exceptions, _version = _load_rules()

    matching_exception = None
    for offset in range(len(labels)):
        candidate = ".".join(labels[offset:])
        if candidate in exceptions:
            matching_exception = candidate
            break
    if matching_exception is not None:
        suffix_labels = max(1, len(matching_exception.split(".")) - 1)
        return ".".join(labels[-suffix_labels:])

    # The implicit prevailing rule is "*".
    prevailing_labels = 1
    for offset in range(len(labels)):
        rule = ".".join(labels[offset:])
        if rule in exact:
            prevailing_labels = max(
                prevailing_labels, len(rule.split(".")))
        if offset > 0 and rule in wildcards:
            suffix_labels = len(rule.split("."))
            prevailing_labels = max(
                prevailing_labels, suffix_labels + 1)
    return ".".join(labels[-prevailing_labels:])


@lru_cache(maxsize=4096)
def registrable_domain(host):
    """Return eTLD+1, or None when the host itself is a public suffix."""
    host = canonical_host(host)
    if not host:
        return None
    try:
        ipaddress.ip_address(host)
        return host
    except ValueError:
        pass
    suffix = public_suffix(host)
    labels = host.split(".")
    suffix_labels = suffix.split(".") if suffix else []
    if len(labels) <= len(suffix_labels):
        return None
    return ".".join(labels[-len(suffix_labels) - 1:])


def site_domain(host):
    """Domain component used by a schemeful site (IP/local names included)."""
    host = canonical_host(host)
    return registrable_domain(host) or host


def is_public_suffix(host):
    host = canonical_host(host)
    return bool(host) and public_suffix(host) == host
