"""Browser-side renderer process facade.

The implementation currently shares one module with the spawn entry point so
Windows can import a single stable target. Callers import this browser-owned
facade; the renderer child receives only its pipe handle and renderer id.
"""

from .renderer_host import (
    RemoteRendererError,
    RendererCrashed,
    RendererHung,
    RendererProcessError,
    RendererProcessHost,
)

__all__ = [
    "RemoteRendererError",
    "RendererCrashed",
    "RendererHung",
    "RendererProcessError",
    "RendererProcessHost",
]
