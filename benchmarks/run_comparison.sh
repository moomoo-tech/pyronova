#!/usr/bin/env bash
# Head-to-head: Pyre vs FastAPI
# Usage: bash benchmarks/run_comparison.sh
set -e

WRK_CMD="wrk"
if ! command -v $WRK_CMD &>/dev/null; then
    echo "❌ wrk not found. Install: brew install wrk"
    exit 1
fi

DURATION="10s"
THREADS=4
CONNECTIONS=100

ORDER_JSON='{"symbol":"AAPL","quantity":100,"price":185.50,"side":"buy"}'

echo "╔══════════════════════════════════════════════════════════╗"
echo "║        Pyre vs FastAPI — Head-to-Head Benchmark         ║"
echo "╚══════════════════════════════════════════════════════════╝"
echo ""
echo "Config: ${THREADS} threads, ${CONNECTIONS} connections, ${DURATION} per test"
echo ""

# ─── Helper ──────────────────────────────────────────────────
run_wrk() {
    local label="$1"
    local url="$2"
    local method="$3"
    local body="$4"

    if [ "$method" = "POST" ]; then
        # Create a lua script for POST requests
        LUA_FILE=$(mktemp /tmp/wrk_post_XXXXXX.lua)
        cat > "$LUA_FILE" << EOLUA
wrk.method = "POST"
wrk.headers["Content-Type"] = "application/json"
wrk.body = '$body'
EOLUA
        result=$($WRK_CMD -t$THREADS -c$CONNECTIONS -d$DURATION -s "$LUA_FILE" "$url" 2>&1)
        rm -f "$LUA_FILE"
    else
        result=$($WRK_CMD -t$THREADS -c$CONNECTIONS -d$DURATION "$url" 2>&1)
    fi

    rps=$(echo "$result" | grep "Requests/sec" | awk '{print $2}')
    latency=$(echo "$result" | grep "Latency" | awk '{print $2}')
    printf "  %-12s │ %10s req/s │ latency %s\n" "$label" "$rps" "$latency"
}

run_test() {
    local test_name="$1"
    local path="$2"
    local method="${3:-GET}"
    local body="$4"

    echo "── $test_name ──"
    run_wrk "FastAPI" "http://127.0.0.1:8000${path}" "$method" "$body"
    run_wrk "Pyre" "http://127.0.0.1:8001${path}" "$method" "$body"
    echo ""
}

# ─── Start servers ───────────────────────────────────────────
echo "Starting FastAPI on :8000..."
uvicorn benchmarks.fastapi_bench_app:app --host 127.0.0.1 --port 8000 --workers 1 --log-level error &
FASTAPI_PID=$!

echo "Starting Pyre on :8001..."
python benchmarks/pyre_bench_app.py &
PYRE_PID=$!

sleep 3

# Verify both are running
curl -sf http://127.0.0.1:8000/health > /dev/null || { echo "❌ FastAPI not running"; kill $PYRE_PID 2>/dev/null; exit 1; }
curl -sf http://127.0.0.1:8001/health > /dev/null || { echo "❌ Pyre not running"; kill $FASTAPI_PID 2>/dev/null; exit 1; }

echo "Both servers running. Starting benchmarks..."
echo ""

# ─── Run benchmarks ──────────────────────────────────────────
run_test "1. Health Check (plain JSON)" "/health" "GET"
run_test "2. JSON Echo (parse + serialize)" "/echo" "POST" "$ORDER_JSON"
run_test "3. CPU-bound (moving average)" "/compute" "POST" ""
run_test "4. Validation" "/validate" "POST" "$ORDER_JSON"

# ─── Memory comparison ──────────────────────────────────────
echo "── Memory Usage ──"
FASTAPI_MEM=$(ps -o rss= -p $FASTAPI_PID 2>/dev/null | awk '{printf "%.1f", $1/1024}')
PYRE_MEM=$(ps -o rss= -p $PYRE_PID 2>/dev/null | awk '{printf "%.1f", $1/1024}')
printf "  %-12s │ %s MB\n" "FastAPI" "$FASTAPI_MEM"
printf "  %-12s │ %s MB\n" "Pyre" "$PYRE_MEM"

# ─── Cleanup ─────────────────────────────────────────────────
kill $FASTAPI_PID $PYRE_PID 2>/dev/null
wait $FASTAPI_PID $PYRE_PID 2>/dev/null

echo ""
echo "Done."
