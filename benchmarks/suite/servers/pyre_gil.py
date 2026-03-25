"""Pyre GIL mode benchmark server."""
from pyreframework import PyreApp
import json

app = PyreApp()

def t1_hello(req):
    return "Hello"

def t2_json_small(req):
    return {"key": "value", "num": 42, "flag": True}

def t3_json_medium(req):
    return {"users": [{"id": i, "name": f"user_{i}", "email": f"user{i}@test.com", "score": i * 1.5} for i in range(100)]}

def t4_json_large(req):
    return {"records": [{"id": i, "data": f"payload_{i}" * 5, "values": [j * 0.1 for j in range(10)]} for i in range(500)]}

def _fib(n):
    if n < 2: return n
    return _fib(n-1) + _fib(n-2)

def c1_fib10(req):
    return {"result": _fib(10)}

def c2_fib20(req):
    return {"result": _fib(20)}

def c3_fib30(req):
    return {"result": _fib(30)}

def p1_pure_python(req):
    return {"result": sum(range(10000))}

def p2_numpy(req):
    import numpy as np
    return {"result": float(np.mean(np.random.randn(10000)))}

def p3_numpy_heavy(req):
    import numpy as np
    m = np.random.randn(100, 100)
    np.linalg.svd(m)
    return {"done": True}

def i1_sleep(req):
    import time
    time.sleep(0.001)
    return "ok"

def j1_parse_small(req):
    data = json.loads(req.text())
    return data

def j2_parse_medium(req):
    data = json.loads(req.text())
    return {"count": len(data.get("users", []))}

def j3_parse_large(req):
    data = json.loads(req.text())
    return {"count": len(data.get("records", []))}


app.get("/t1", t1_hello)
app.get("/t2", t2_json_small)
app.get("/t3", t3_json_medium)
app.get("/t4", t4_json_large)
app.get("/c1", c1_fib10)
app.get("/c2", c2_fib20)
app.get("/c3", c3_fib30)
app.get("/p1", p1_pure_python)
app.get("/p2", p2_numpy)
app.get("/p3", p3_numpy_heavy)
app.get("/i1", i1_sleep)
app.post("/j1", j1_parse_small)
app.post("/j2", j2_parse_medium)
app.post("/j3", j3_parse_large)

if __name__ == "__main__":
    app.run(host="127.0.0.1", port=9000)
