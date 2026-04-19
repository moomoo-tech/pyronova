#!/usr/bin/env bash
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

source .venv/bin/activate

THREADS=4
CONNECTIONS=256
DURATION="10s"

cleanup() {
    echo ""
    echo "Cleaning up..."
    lsof -ti:8000 | xargs kill -9 2>/dev/null || true
    lsof -ti:8001 | xargs kill -9 2>/dev/null || true
}
trap cleanup EXIT

# Kill any leftover processes
cleanup 2>/dev/null

echo "============================================================"
echo "  SkyTrade Engine vs Robyn — Head-to-Head Benchmark"
echo "  wrk -t${THREADS} -c${CONNECTIONS} -d${DURATION}"
echo "============================================================"

# --- SkyTrade ---
echo ""
echo ">>> Starting SkyTrade Engine on :8000 ..."
python benchmarks/bench_plaintext.py &
SKY_PID=$!
sleep 2

echo ""
echo "--- [SkyTrade] GET / (plain text) ---"
wrk -t${THREADS} -c${CONNECTIONS} -d${DURATION} http://127.0.0.1:8000/
echo ""
echo "--- [SkyTrade] GET /hello/bench (JSON-like) ---"
wrk -t${THREADS} -c${CONNECTIONS} -d${DURATION} http://127.0.0.1:8000/hello/bench

kill $SKY_PID 2>/dev/null || true
sleep 1

# --- Robyn ---
echo ""
echo ">>> Starting Robyn on :8001 ..."
python benchmarks/robyn_app.py &
ROBYN_PID=$!
sleep 3

echo ""
echo "--- [Robyn] GET / (plain text) ---"
wrk -t${THREADS} -c${CONNECTIONS} -d${DURATION} http://127.0.0.1:8001/
echo ""
echo "--- [Robyn] GET /hello/bench (JSON-like) ---"
wrk -t${THREADS} -c${CONNECTIONS} -d${DURATION} http://127.0.0.1:8001/hello/bench

kill $ROBYN_PID 2>/dev/null || true

echo ""
echo "============================================================"
echo "  Benchmark complete!"
echo "============================================================"
