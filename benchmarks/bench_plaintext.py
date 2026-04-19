"""Pyre TFB-style benchmark target.

Feature-light so `just bench-record` measures pure engine cost
(routing + request/response dispatch), not optional add-ons. The
shape of the `/` route mirrors the TechEmpower Web Framework
Benchmarks "Plaintext" test.

For a feature-rich example (middleware, access log, static files,
custom 404), see `examples/hello.py`.
"""

from pyreframework import Pyre, PyreResponse

app = Pyre()


# ---------------------------------------------------------------------------
# Routes
# ---------------------------------------------------------------------------

@app.get("/")
def index(req):
    return "Hello from Pyre!"


@app.get("/hello/{name}")
def greet(req):
    name = req.params.get("name", "world")
    return {"message": f"Hello, {name}!"}


@app.post("/echo")
def echo(req):
    return req.text()


@app.get("/search")
def search(req):
    """Query params + request headers."""
    q = req.query_params.get("q", "")
    user_agent = req.headers.get("user-agent", "unknown")
    return {"query": q, "user_agent": user_agent}


@app.get("/html")
def html_page(req):
    """Custom content-type."""
    return PyreResponse(
        body="<h1>Hello from Pyre</h1>",
        content_type="text/html; charset=utf-8",
    )


@app.fallback
def not_found(req):
    """Custom 404 handler."""
    return PyreResponse(
        body={"error": "not found", "path": req.path},
        status_code=404,
        content_type="application/json",
    )


if __name__ == "__main__":
    app.run(host="127.0.0.1", port=8000)
