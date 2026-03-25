"""Cookie utilities for Pyre.

Read cookies from request headers, set cookies on responses.

Usage::

    from pyreframework.cookies import get_cookies, set_cookie

    @app.get("/")
    def index(req):
        cookies = get_cookies(req)
        session = cookies.get("session_id", "none")
        return set_cookie(
            PyreResponse(body=f"session={session}"),
            "session_id", "abc123",
            max_age=3600, httponly=True,
        )
"""

from __future__ import annotations
from typing import Optional


def get_cookies(req) -> dict[str, str]:
    """Parse cookies from request headers.

    Returns a dict of cookie name → value.
    """
    cookie_header = req.headers.get("cookie", "")
    if not cookie_header:
        return {}
    cookies = {}
    for pair in cookie_header.split(";"):
        pair = pair.strip()
        if "=" in pair:
            name, _, value = pair.partition("=")
            cookies[name.strip()] = value.strip()
    return cookies


def get_cookie(req, name: str, default: str | None = None) -> str | None:
    """Get a single cookie value by name."""
    return get_cookies(req).get(name, default)


def set_cookie(
    response,
    name: str,
    value: str,
    *,
    max_age: int | None = None,
    expires: str | None = None,
    path: str = "/",
    domain: str | None = None,
    secure: bool = False,
    httponly: bool = False,
    samesite: str | None = "Lax",
) -> "PyreResponse":
    """Set a cookie on a PyreResponse.

    Returns a new PyreResponse with the Set-Cookie header added.
    """
    from pyreframework.engine import PyreResponse

    parts = [f"{name}={value}"]
    if max_age is not None:
        parts.append(f"Max-Age={max_age}")
    if expires:
        parts.append(f"Expires={expires}")
    if path:
        parts.append(f"Path={path}")
    if domain:
        parts.append(f"Domain={domain}")
    if secure:
        parts.append("Secure")
    if httponly:
        parts.append("HttpOnly")
    if samesite:
        parts.append(f"SameSite={samesite}")

    cookie_str = "; ".join(parts)
    headers = dict(getattr(response, "headers", {}) or {})
    headers["set-cookie"] = cookie_str

    return PyreResponse(
        body=response.body,
        status_code=getattr(response, "status_code", 200),
        content_type=getattr(response, "content_type", None),
        headers=headers,
    )


def delete_cookie(response, name: str, *, path: str = "/") -> "PyreResponse":
    """Delete a cookie by setting it expired."""
    return set_cookie(
        response, name, "",
        max_age=0, path=path,
    )
