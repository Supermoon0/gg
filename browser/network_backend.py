"""Process-neutral network contract and the current in-process backend.

Product code depends on this small surface instead of importing the mutable
cookie/cache/socket globals in :mod:`browser.net` directly.  A later IPC
backend can implement the same contract in the browser process while the
network service owns those globals.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Protocol, runtime_checkable
import uuid

from . import net


@runtime_checkable
class NetworkBackend(Protocol):
    """Network operations needed by navigation and a renderer session."""

    def bind_context(self, document_url, **kwargs): ...

    def drop_context(self, context): ...

    def new_cancel_token(self): ...

    def cancel(self, token): ...

    def request_text(self, url, **kwargs): ...

    def request(self, url, **kwargs): ...

    def request_raw(self, url, **kwargs): ...

    def perform_script_fetch(self, base_url, request, **kwargs): ...

    def cookies_for(self, url, **kwargs): ...

    def set_cookie_from_js(self, url, value, **kwargs): ...


@dataclass(frozen=True)
class NetworkContext:
    """Serializable identity for one committed document's network rights."""

    context_id: str
    profile_id: str
    document_url: str


class LocalNetworkBackend:
    """Adapter over the existing local HTTP/cookie/cache implementation."""

    @staticmethod
    def _local_kwargs(kwargs):
        kwargs.pop("context", None)
        return kwargs

    def bind_context(self, document_url, *, profile_id="default"):
        return NetworkContext(
            uuid.uuid4().hex, str(profile_id), str(document_url))

    def drop_context(self, context):
        return context is not None

    def new_cancel_token(self):
        return net.CancellationToken()

    def cancel(self, token):
        return token.cancel()

    def request_text(self, url, **kwargs):
        return net.request_text(url, **self._local_kwargs(kwargs))

    def request(self, url, **kwargs):
        return net.request(url, **self._local_kwargs(kwargs))

    def request_raw(self, url, **kwargs):
        return net.request_raw(url, **self._local_kwargs(kwargs))

    def perform_script_fetch(self, base_url, request, **kwargs):
        from .security import perform_script_fetch

        return perform_script_fetch(
            base_url, request, **self._local_kwargs(kwargs))

    def cookies_for(self, url, **kwargs):
        return net.cookies_for(url)

    def set_cookie_from_js(self, url, value, **kwargs):
        return net.set_cookie_from_js(url, value)


_LOCAL_BACKEND = LocalNetworkBackend()


def default_network_backend():
    """Return the shared local backend used until an IPC client is selected."""
    return _LOCAL_BACKEND
