"""FastAPI benchmark app — traditional stack (Pydantic + json)."""

from fastapi import FastAPI
from pydantic import BaseModel
import json

app = FastAPI()


class Order(BaseModel):
    symbol: str
    quantity: int
    price: float
    side: str = "buy"


# 1. Plain JSON response
@app.get("/health")
def health():
    return {"status": "ok", "framework": "fastapi"}


# 2. JSON echo (parse + serialize)
@app.post("/echo")
def echo(order: Order):
    return order.model_dump()


# 3. CPU-bound: compute moving average
@app.post("/compute")
def compute():
    data = list(range(1, 10001))
    window = 50
    result = []
    for i in range(len(data) - window + 1):
        avg = sum(data[i : i + window]) / window
        result.append(round(avg, 2))
    return {"count": len(result), "last": result[-1]}


# 4. Multi-field validation
@app.post("/validate")
def validate(order: Order):
    errors = []
    if order.quantity <= 0:
        errors.append("quantity must be positive")
    if order.price <= 0:
        errors.append("price must be positive")
    if order.side not in ("buy", "sell"):
        errors.append("side must be buy or sell")
    if errors:
        return {"valid": False, "errors": errors}
    return {"valid": True, "total": order.quantity * order.price}
