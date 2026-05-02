"""Benchmark: sub-interpreter mode with headers + query_params."""

import json
from pyronova import PyronovaApp

app = PyronovaApp()


def index(req):
    return "Hello from Pyronova!"


def greet(req):
    name = req.params.get("name", "world")
    return json.dumps({"message": f"Hello, {name}!"})


def search(req):
    q = req.query_params.get("q", "")
    ua = req.headers.get("user-agent", "unknown")
    return json.dumps({"query": q, "user_agent": ua})


app.get("/", index)
app.get("/hello/{name}", greet)
app.get("/search", search)

if __name__ == "__main__":
    app.run(host="127.0.0.1", port=8000, mode="subinterp")
