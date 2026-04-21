"""Request-scoped context — carry values through handlers and hooks.

Usage::

    from pyreframework.context import ctx

    @app.before_request
    def tag(req):
        ctx.set("user_id", req.headers.get("x-user"))

    @app.get("/me")
    def me(req):
        return {"user": ctx.get("user_id"), "trace": ctx.request_id()}

The context is a per-request dictionary. Values set during the request
are visible to every hook and helper called from the same thread or
awaited coroutine, and cleared before the next request begins.

Under the hood:

- Backed by a ``ContextVar[dict]`` so async handlers inherit the scope
  across ``await`` boundaries without extra plumbing.
- Each before-request hook (installed by ``reset_context_on_request``,
  which Pyre wires automatically when you enable request-id or metrics)
  replaces the stored dict, so leftover keys from a recycled worker
  thread never leak.

``request_id()`` is a dedicated accessor because it's the canonical
correlation ID everyone needs and we don't want every caller to know
the magic key. Other values live under user-chosen keys.
"""

from __future__ import annotations

from contextvars import ContextVar
from typing import Any


_REQUEST_ID_KEY = "__pyre_request_id__"

_current: ContextVar[dict[str, Any]] = ContextVar("pyre_ctx", default={})


class _Ctx:
    """Facade over the ``ContextVar``. Module-level ``ctx`` is the only
    instance users need."""

    def get(self, key: str, default: Any = None) -> Any:
        return _current.get().get(key, default)

    def set(self, key: str, value: Any) -> None:
        # Copy-on-write: never mutate the dict stored in an outer scope
        # (e.g., the default empty dict shared by every worker thread
        # before any request has started).
        d = _current.get()
        if d is _DEFAULT:
            d = {}
        else:
            d = dict(d)
        d[key] = value
        _current.set(d)

    def clear(self) -> None:
        _current.set({})

    def request_id(self) -> str | None:
        return self.get(_REQUEST_ID_KEY)

    def set_request_id(self, rid: str) -> None:
        self.set(_REQUEST_ID_KEY, rid)

    def snapshot(self) -> dict[str, Any]:
        """Return a shallow copy of the current context dict."""
        return dict(_current.get())


_DEFAULT: dict[str, Any] = {}
ctx = _Ctx()


def _reset_for_new_request() -> None:
    """Called by Pyre's internal before-request hook to start each
    request with a fresh dict."""
    _current.set({})


__all__ = ["ctx"]
