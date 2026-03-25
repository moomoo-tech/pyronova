"""Pyre sub-interpreter mode example.

Each worker thread gets its own Python interpreter with its own GIL.
True parallelism without free-threaded Python.

Usage:
    python examples/hello_subinterp.py
"""

from pyreframework import PyreApp

app = PyreApp()


def index(req):
    return "Hello from Pyre (sub-interpreter)!"


def greet(req):
    name = req.params.get("name", "world")
    return {"message": f"Hello, {name}!"}


def echo(req):
    return req.text()


app.get("/", index)
app.get("/hello/{name}", greet)
app.post("/echo", echo)

if __name__ == "__main__":
    app.run(host="127.0.0.1", port=8000, mode="subinterp")
