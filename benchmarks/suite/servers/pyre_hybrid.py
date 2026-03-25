"""Pyre hybrid mode benchmark server — numpy routes on GIL, rest on sub-interp."""
from pyreframework import Pyre
import json

app = Pyre()

@app.get("/t1")
def t1_hello(req):
    return "Hello"

@app.get("/t2")
def t2_json_small(req):
    return {"key": "value", "num": 42, "flag": True}

@app.get("/t3")
def t3_json_medium(req):
    return {"users": [{"id": i, "name": f"user_{i}", "email": f"user{i}@test.com", "score": i * 1.5} for i in range(100)]}

@app.get("/t4")
def t4_json_large(req):
    return {"records": [{"id": i, "data": f"payload_{i}" * 5, "values": [j * 0.1 for j in range(10)]} for i in range(500)]}

def _fib(n):
    if n < 2: return n
    return _fib(n-1) + _fib(n-2)

@app.get("/c1")
def c1_fib10(req):
    return {"result": _fib(10)}

@app.get("/c2")
def c2_fib20(req):
    return {"result": _fib(20)}

@app.get("/c3")
def c3_fib30(req):
    return {"result": _fib(30)}

@app.get("/p1")
def p1_pure_python(req):
    return {"result": sum(range(10000))}

@app.get("/p2", gil=True)
def p2_numpy(req):
    import numpy as np
    return {"result": float(np.mean(np.random.randn(10000)))}

@app.get("/p3", gil=True)
def p3_numpy_heavy(req):
    import numpy as np
    m = np.random.randn(100, 100)
    np.linalg.svd(m)
    return {"done": True}

@app.get("/i1")
def i1_sleep(req):
    import time
    time.sleep(0.001)
    return "ok"

@app.post("/j1")
def j1_parse_small(req):
    data = json.loads(req.text())
    return data

@app.post("/j2")
def j2_parse_medium(req):
    data = json.loads(req.text())
    return {"count": len(data.get("users", []))}

@app.post("/j3")
def j3_parse_large(req):
    data = json.loads(req.text())
    return {"count": len(data.get("records", []))}

if __name__ == "__main__":
    app.run(host="127.0.0.1", port=9000, mode="subinterp")
