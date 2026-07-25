"""Marshalling for the network kwargs that cross a process boundary.

Every hop that forwards a fetch has to move the same keyword arguments,
and they are not all JSON scalars. `site_for_cookies` is a `net.URL`,
and dropping it is not a lost hint: `net` reads a missing initiator as
*same site*, so SameSite cookies would ride cross-site subresource
loads and mixed content would stop being blocked.

That bug happened once because the marshalling lived inline at one hop.
It lives here now so every hop shares one implementation.
"""

from __future__ import annotations

from .. import net

_JSON_SCALARS = (str, int, float, bool, dict, list)


def _is_url(value):
    return (value is not None
            and hasattr(value, "scheme") and hasattr(value, "host"))


def to_wire(kwargs, *, context_key="context_id"):
    """Network kwargs -> a JSON-safe payload.

    URL-valued arguments travel as strings under `url_kwargs`; the
    caller's live cancel token and bound context are replaced by the
    ids the far side knows them by.
    """
    kwargs = dict(kwargs)
    context = kwargs.pop("context", None)
    kwargs.pop("cancel_token", None)
    urls = {key: str(kwargs.pop(key))
            for key in list(kwargs) if _is_url(kwargs[key])}
    clean = {key: value for key, value in kwargs.items()
             if value is None or isinstance(value, _JSON_SCALARS)}
    if urls:
        clean["url_kwargs"] = urls
    if context is not None:
        clean[context_key] = (context.get("context_id")
                              if isinstance(context, dict)
                              else getattr(context, "context_id", None))
    return clean


def from_wire(payload, *, contexts=None, cancel_token=None,
              context_key="context_id"):
    """The inverse: rebuild URLs, and re-bind the context and token.

    An unknown context id is a protocol error rather than a silent
    downgrade to unbound — the caller named rights it must actually
    hold.
    """
    from .protocol import ProtocolError

    kwargs = dict(payload or {})
    for key, value in (kwargs.pop("url_kwargs", None) or {}).items():
        kwargs[key] = net.URL(value)
    context_id = kwargs.pop(context_key, None)
    if context_id is not None and contexts is not None:
        context = contexts.get(context_id)
        if context is None:
            raise ProtocolError("invalid_context", "unknown network context")
        kwargs["context"] = context
    if cancel_token is not None:
        kwargs["cancel_token"] = cancel_token
    return kwargs
