"""
Trading Data API — demonstrates Pyronova for quantitative finance.

Features used:
  - numpy + pandas computation (gil=True hybrid dispatch)
  - WebSocket (live price streaming)
  - SharedState (cross-worker market data cache)
  - async handlers (concurrent external API calls)
  - MsgPack RPC (binary-efficient inter-service communication)
  - Pydantic validation (order schema)
  - CORS + structured logging

Run:
  pip install numpy pandas
  python examples/trading_api.py

Test:
  # Market snapshot
  curl http://127.0.0.1:8000/market/AAPL

  # Submit order (Pydantic validated)
  curl -X POST http://127.0.0.1:8000/order -H 'Content-Type: application/json' \
    -d '{"ticker": "AAPL", "side": "buy", "quantity": 100, "price": 150.5}'

  # Portfolio analytics (numpy + pandas)
  curl http://127.0.0.1:8000/analytics/portfolio

  # WebSocket price stream (use websocat or browser)
  # websocat ws://127.0.0.1:8000/ws/prices

  # RPC call
  python -c "
  from pyronova import RPCClient
  with RPCClient('http://127.0.0.1:8000') as c:
      print(c.get_signals(tickers=['AAPL', 'TSLA']))
  "
"""

import json
import time
import random
import threading
from pydantic import BaseModel, Field
from pyronova import Pyronova, Response

app = Pyronova()
app.enable_cors()
app.enable_logging()


# ---------------------------------------------------------------------------
# Models
# ---------------------------------------------------------------------------

class OrderRequest(BaseModel):
    ticker: str = Field(max_length=5)
    side: str = Field(pattern="^(buy|sell)$")
    quantity: int = Field(gt=0, le=100000)
    price: float = Field(gt=0)


# ---------------------------------------------------------------------------
# Simulated market data
# ---------------------------------------------------------------------------

TICKERS = ["AAPL", "TSLA", "NVDA", "MSFT", "AMZN", "GOOG", "META", "JPM", "V", "WMT"]
BASE_PRICES = {t: random.uniform(50, 500) for t in TICKERS}


def get_live_price(ticker: str) -> dict:
    base = BASE_PRICES.get(ticker, 100.0)
    price = base * (1 + random.gauss(0, 0.002))
    return {
        "ticker": ticker,
        "price": round(price, 2),
        "bid": round(price * 0.999, 2),
        "ask": round(price * 1.001, 2),
        "volume": random.randint(10000, 1000000),
        "timestamp": time.time(),
    }


# ---------------------------------------------------------------------------
# Initialize market data in SharedState
# ---------------------------------------------------------------------------

def init_market_data():
    for ticker in TICKERS:
        data = get_live_price(ticker)
        app.state[f"price:{ticker}"] = json.dumps(data)

    # Simulated portfolio
    portfolio = [
        {"ticker": "AAPL", "shares": 500, "avg_cost": 145.0},
        {"ticker": "TSLA", "shares": 200, "avg_cost": 220.0},
        {"ticker": "NVDA", "shares": 100, "avg_cost": 450.0},
        {"ticker": "MSFT", "shares": 300, "avg_cost": 380.0},
    ]
    app.state["portfolio"] = json.dumps(portfolio)


# ---------------------------------------------------------------------------
# Routes
# ---------------------------------------------------------------------------

@app.get("/")
def index(req):
    return {
        "service": "Pyronova Trading API",
        "tickers": TICKERS,
        "endpoints": [
            "GET /market/{ticker} — live price quote",
            "GET /market — all tickers",
            "POST /order — submit order (Pydantic validated)",
            "GET /analytics/portfolio — numpy/pandas portfolio analysis",
            "GET /analytics/correlation — cross-asset correlation matrix",
            "WS /ws/prices — live WebSocket price stream",
            "RPC /rpc/get_signals — trading signals via MsgPack",
        ],
    }


@app.get("/market/{ticker}", gil=True)
def market_quote(req):
    """Get live price for a single ticker. Uses SharedState cache."""
    ticker = req.params["ticker"].upper()
    # Update price in SharedState
    data = get_live_price(ticker)
    app.state[f"price:{ticker}"] = json.dumps(data)
    return data


@app.get("/market", gil=True)
def market_all(req):
    """Get all ticker prices."""
    quotes = {}
    for ticker in TICKERS:
        data = get_live_price(ticker)
        app.state[f"price:{ticker}"] = json.dumps(data)
        quotes[ticker] = data
    return {"quotes": quotes, "count": len(quotes)}


@app.post("/order", model=OrderRequest, gil=True)
def submit_order(req, order: OrderRequest):
    """Submit a trade order. Pydantic validates ticker, side, quantity, price."""
    # Get current price
    current = get_live_price(order.ticker)

    # Simple fill simulation
    slippage = random.uniform(0, 0.001)
    fill_price = order.price * ((1 + slippage) if order.side == "buy" else (1 - slippage))

    return {
        "status": "filled",
        "order_id": f"ORD-{random.randint(100000, 999999)}",
        "ticker": order.ticker,
        "side": order.side,
        "quantity": order.quantity,
        "requested_price": order.price,
        "fill_price": round(fill_price, 2),
        "market_price": current["price"],
        "slippage_bps": round(slippage * 10000, 1),
        "timestamp": time.time(),
    }


@app.get("/analytics/portfolio", gil=True)
def portfolio_analytics(req):
    """Portfolio analytics using numpy and pandas. Runs on main interpreter (gil=True)."""
    import numpy as np

    portfolio = json.loads(app.state.get("portfolio") or "[]")
    if not portfolio:
        return {"error": "No portfolio data"}

    # Get current prices
    for pos in portfolio:
        current = get_live_price(pos["ticker"])
        pos["current_price"] = current["price"]
        pos["market_value"] = round(pos["shares"] * current["price"], 2)
        pos["cost_basis"] = round(pos["shares"] * pos["avg_cost"], 2)
        pos["pnl"] = round(pos["market_value"] - pos["cost_basis"], 2)
        pos["pnl_pct"] = round((pos["current_price"] / pos["avg_cost"] - 1) * 100, 2)

    # numpy aggregation
    values = np.array([p["market_value"] for p in portfolio])
    pnls = np.array([p["pnl"] for p in portfolio])
    weights = values / values.sum()

    return {
        "positions": portfolio,
        "summary": {
            "total_value": round(float(values.sum()), 2),
            "total_pnl": round(float(pnls.sum()), 2),
            "avg_pnl_pct": round(float(np.mean([p["pnl_pct"] for p in portfolio])), 2),
            "sharpe_estimate": round(float(np.mean(pnls) / (np.std(pnls) + 1e-8)), 4),
            "max_position_weight": round(float(weights.max()) * 100, 1),
            "num_positions": len(portfolio),
        },
    }


@app.get("/analytics/correlation", gil=True)
def correlation_matrix(req):
    """Cross-asset correlation using numpy. Simulated returns."""
    import numpy as np

    n_days = 60
    tickers = ["AAPL", "TSLA", "NVDA", "MSFT"]
    returns = {t: np.random.randn(n_days) * 0.02 for t in tickers}

    corr = np.corrcoef([returns[t] for t in tickers])
    return {
        "tickers": tickers,
        "correlation_matrix": [[round(float(c), 3) for c in row] for row in corr],
        "days": n_days,
    }


# ---------------------------------------------------------------------------
# WebSocket — live price stream
# ---------------------------------------------------------------------------

@app.websocket("/ws/prices")
def price_stream(ws):
    """Stream live prices every 500ms via WebSocket."""
    try:
        while True:
            msg = ws.recv()
            if msg is None:
                break

            # Client can send ticker filter: "AAPL,TSLA" or "all"
            if msg.strip().lower() == "all":
                tickers = TICKERS
            else:
                tickers = [t.strip().upper() for t in msg.split(",")]

            quotes = {t: get_live_price(t) for t in tickers if t in TICKERS}
            ws.send(json.dumps(quotes))
    except Exception:
        pass


# ---------------------------------------------------------------------------
# RPC — binary-efficient inter-service calls
# ---------------------------------------------------------------------------

@app.rpc("/rpc/get_signals")
def get_signals(data):
    """Generate trading signals. Called via MsgPack RPC from other services."""
    tickers = data.get("tickers", TICKERS[:5])
    signals = []
    for ticker in tickers:
        price = get_live_price(ticker)
        signal = random.choice(["buy", "sell", "hold"])
        confidence = round(random.uniform(0.5, 0.99), 2)
        signals.append({
            "ticker": ticker,
            "signal": signal,
            "confidence": confidence,
            "price": price["price"],
        })
    return {"signals": signals, "timestamp": time.time()}


# ---------------------------------------------------------------------------
# Start
# ---------------------------------------------------------------------------

if __name__ == "__main__":
    init_market_data()
    app.run()
