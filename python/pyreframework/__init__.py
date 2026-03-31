"""Pyre — A high-performance Python web framework powered by Rust."""

from pyreframework.engine import PyreApp, PyreRequest, PyreResponse, PyreWebSocket, SharedState, PyreStream, get_gil_metrics, init_logger, emit_python_log
from pyreframework.app import Pyre
from pyreframework.rpc import PyreRPCClient
from pyreframework.cookies import get_cookies, get_cookie, set_cookie, delete_cookie
from pyreframework.uploads import parse_multipart, UploadFile


def redirect(url: str, status_code: int = 302) -> PyreResponse:
    """Return a redirect response.

    Usage::

        @app.get("/old")
        def old_page(req):
            return redirect("/new")

        @app.get("/permanent")
        def moved(req):
            return redirect("/new-home", status_code=301)
    """
    return PyreResponse(
        body="",
        status_code=status_code,
        headers={"location": url},
    )

__all__ = ["Pyre", "PyreApp", "PyreRequest", "PyreResponse", "PyreWebSocket", "SharedState", "PyreStream", "get_gil_metrics", "init_logger", "emit_python_log"]
try:
    from importlib.metadata import version as _get_version
    __version__ = _get_version("pyreframework")
except Exception:
    __version__ = "dev"
