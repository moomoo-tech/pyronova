"""Cookie utilities for Pyronova.

Read cookies from request headers, set cookies on responses.

Usage::

    from pyronova.cookies import get_cookies, set_cookie

    @app.get("/")
    def index(req):
        cookies = get_cookies(req)
        session = cookies.get("session_id", "none")
        return set_cookie(
            Response(body=f"session={session}"),
            "session_id", "abc123",
            max_age=3600, httponly=True,
        )
"""

from __future__ import annotations
from typing import Optional, TYPE_CHECKING

if TYPE_CHECKING:
    from pyronova.engine import Request, Response

# Characters forbidden in cookie name/value per RFC 6265. CR (\r) and LF
# (\n) in particular enable HTTP Response Splitting: an attacker crafts a
# value containing `\r\nSet-Cookie: admin=1` and injects arbitrary headers
# into the response. NUL is a control-char trap too. We reject rather than
# silently escape — cookies with these bytes are always the result of
# unsanitized user input reaching set_cookie(), and silent acceptance
# (e.g. encoding) would mask the real bug upstream.
_COOKIE_FORBIDDEN = ("\r", "\n", "\0")


def _reject_control_chars(field: str, value: str) -> None:
    for ch in _COOKIE_FORBIDDEN:
        if ch in value:
            raise ValueError(
                f"cookie {field} contains forbidden control character "
                f"{ch!r}; refusing to emit (HTTP response splitting risk)"
            )


def get_cookies(req: Request) -> dict[str, str]:
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


def get_cookie(req: Request, name: str, default: str | None = None) -> str | None:
    """Get a single cookie value by name."""
    return get_cookies(req).get(name, default)


def set_cookie(
    response: Response,
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
) -> "Response":
    """Set a cookie on a Response.

    Returns a new Response with the Set-Cookie header added.
    """
    from pyronova.engine import Response

    _reject_control_chars("name", name)
    _reject_control_chars("value", value)
    if domain is not None:
        _reject_control_chars("domain", domain)
    if path:
        _reject_control_chars("path", path)
    if expires is not None:
        _reject_control_chars("expires", expires)
    if samesite is not None:
        _reject_control_chars("samesite", samesite)

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

    return Response(
        body=response.body,
        status_code=getattr(response, "status_code", 200),
        content_type=getattr(response, "content_type", None),
        headers=headers,
    )


def delete_cookie(response: Response, name: str, *, path: str = "/") -> Response:
    """Delete a cookie by setting it expired."""
    return set_cookie(
        response, name, "",
        max_age=0, path=path,
    )
