"""Robyn benchmark server — equivalent routes."""
from robyn import Robyn
import json

app = Robyn(__file__)

@app.get("/t1")
async def t1_hello(request):
    return "Hello"

@app.get("/t2")
async def t2_json_small(request):
    return json.dumps({"key": "value", "num": 42, "flag": True})

@app.get("/t3")
async def t3_json_medium(request):
    return json.dumps({"users": [{"id": i, "name": f"user_{i}", "email": f"user{i}@test.com", "score": i * 1.5} for i in range(100)]})

@app.get("/t4")
async def t4_json_large(request):
    return json.dumps({"records": [{"id": i, "data": f"payload_{i}" * 5, "values": [j * 0.1 for j in range(10)]} for i in range(500)]})

def _fib(n):
    if n < 2: return n
    return _fib(n-1) + _fib(n-2)

@app.get("/c1")
async def c1_fib10(request):
    return json.dumps({"result": _fib(10)})

@app.get("/c2")
async def c2_fib20(request):
    return json.dumps({"result": _fib(20)})

@app.get("/c3")
async def c3_fib30(request):
    return json.dumps({"result": _fib(30)})

@app.get("/p1")
async def p1_pure_python(request):
    return json.dumps({"result": sum(range(10000))})

@app.get("/p2")
async def p2_numpy(request):
    import numpy as np
    return json.dumps({"result": float(np.mean(np.random.randn(10000)))})

@app.get("/p3")
async def p3_numpy_heavy(request):
    import numpy as np
    m = np.random.randn(100, 100)
    np.linalg.svd(m)
    return json.dumps({"done": True})

@app.get("/i1")
async def i1_sleep(request):
    import asyncio
    await asyncio.sleep(0.001)
    return "ok"

@app.post("/j1")
async def j1_parse_small(request):
    data = json.loads(request.body)
    return json.dumps(data)

@app.post("/j2")
async def j2_parse_medium(request):
    data = json.loads(request.body)
    return json.dumps({"count": len(data.get("users", []))})

@app.post("/j3")
async def j3_parse_large(request):
    data = json.loads(request.body)
    return json.dumps({"count": len(data.get("records", []))})

if __name__ == "__main__":
    app.start(host="127.0.0.1", port=9000)
