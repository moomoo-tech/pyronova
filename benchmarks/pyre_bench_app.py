"""Pyre benchmark app — sub-interpreter safe stack (msgspec + stdlib json)."""

from pyreframework import Pyre
import json

app = Pyre()


# 1. Plain JSON response
@app.get("/health")
def health(req):
    return {"status": "ok", "framework": "pyre"}


# 2. JSON echo (parse + serialize)
@app.post("/echo")
def echo(req):
    data = json.loads(req.body)
    return data


# 3. CPU-bound: compute moving average
@app.post("/compute")
def compute(req):
    data = list(range(1, 10001))
    window = 50
    result = []
    for i in range(len(data) - window + 1):
        avg = sum(data[i : i + window]) / window
        result.append(round(avg, 2))
    return {"count": len(result), "last": result[-1]}


# 4. Manual validation (no Pydantic needed)
@app.post("/validate")
def validate(req):
    order = json.loads(req.body)
    errors = []
    symbol = order.get("symbol", "")
    quantity = order.get("quantity", 0)
    price = order.get("price", 0)
    side = order.get("side", "buy")

    if not isinstance(quantity, int) or quantity <= 0:
        errors.append("quantity must be positive")
    if not isinstance(price, (int, float)) or price <= 0:
        errors.append("price must be positive")
    if side not in ("buy", "sell"):
        errors.append("side must be buy or sell")
    if errors:
        return {"valid": False, "errors": errors}
    return {"valid": True, "total": quantity * price}


if __name__ == "__main__":
    app.run(port=8001)
