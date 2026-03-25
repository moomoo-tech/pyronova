"""Benchmark: sub-interpreter mode with headers + query_params."""

from pyreframework import PyreApp

app = PyreApp()


def index(req):
    return "Hello from Pyre!"


def greet(req):
    name = req.params.get("name", "world")
    return '{"message": "Hello, ' + name + '!"}'


def search(req):
    q = req.query_params.get("q", "")
    ua = req.headers.get("user-agent", "unknown")
    return '{"query": "' + q + '", "user_agent": "' + ua + '"}'


app.get("/", index)
app.get("/hello/{name}", greet)
app.get("/search", search)

if __name__ == "__main__":
    app.run(host="127.0.0.1", port=8000, mode="subinterp")
