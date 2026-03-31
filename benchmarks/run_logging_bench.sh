#!/usr/bin/env bash
# Pyre Logging Performance Benchmark
# Compares 4 logging configurations under identical load:
#   1. No logging (level=OFF) — baseline
#   2. Server log only (level=ERROR) — default production
#   3. Access log enabled (level=INFO + access_log=True)
#   4. User logging in handler (Python logging.info() per request)
#
# Usage: bash benchmarks/run_logging_bench.sh
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_DIR"

source .venv/bin/activate

WRK_CMD="wrk"
if ! command -v $WRK_CMD &>/dev/null; then
    echo "wrk not found. Install: brew install wrk"
    exit 1
fi

THREADS=4
CONNECTIONS=100
DURATION="10s"
PORT=18889

cleanup() {
    lsof -ti:$PORT | xargs kill -9 2>/dev/null || true
}
trap cleanup EXIT
cleanup 2>/dev/null

echo "============================================================"
echo "  Pyre Logging Performance Benchmark"
echo "  wrk -t${THREADS} -c${CONNECTIONS} -d${DURATION}"
echo "  $(date '+%Y-%m-%d %H:%M:%S')"
echo "============================================================"

# --- Generate 4 server scripts ---

# 1. No logging (level=OFF)
cat > /tmp/pyre_bench_log_off.py << 'PYEOF'
from pyreframework import Pyre
app = Pyre(log_config={"level": "OFF"})

@app.get("/")
def index(req): return {"status": "ok"}

app.run(host="127.0.0.1", port=18889)
PYEOF

# 2. Server log only (level=ERROR, no access log) — default production
cat > /tmp/pyre_bench_log_error.py << 'PYEOF'
from pyreframework import Pyre
app = Pyre()  # defaults: level=ERROR, access_log=False

@app.get("/")
def index(req): return {"status": "ok"}

app.run(host="127.0.0.1", port=18889)
PYEOF

# 3. Access log enabled (tracing access log on every request)
cat > /tmp/pyre_bench_log_access.py << 'PYEOF'
from pyreframework import Pyre
app = Pyre(log_config={"level": "INFO", "access_log": True, "format": "text"})

@app.get("/")
def index(req): return {"status": "ok"}

app.run(host="127.0.0.1", port=18889)
PYEOF

# 4. User logging (Python logging.info() in every request handler)
cat > /tmp/pyre_bench_log_user.py << 'PYEOF'
import logging
logger = logging.getLogger("bench")

from pyreframework import Pyre
app = Pyre(log_config={"level": "INFO", "access_log": True, "format": "text"})

@app.get("/")
def index(req):
    logger.info("handling request")
    return {"status": "ok"}

app.run(host="127.0.0.1", port=18889)
PYEOF

# --- Benchmark function ---
run_bench() {
    local label="$1"
    local script="$2"

    echo ""
    echo ">>> [$label] Starting server..."
    python "$script" > /dev/null 2>&1 &
    local PID=$!
    sleep 3

    # Verify server is up
    if ! curl -sf http://127.0.0.1:$PORT/ > /dev/null 2>&1; then
        echo "    Server failed to start!"
        kill $PID 2>/dev/null || true
        return
    fi

    # Record RSS before benchmark
    local RSS_BEFORE=$(ps -o rss= -p $PID 2>/dev/null | awk '{printf "%.1f", $1/1024}')

    # Run wrk
    local RESULT=$($WRK_CMD -t$THREADS -c$CONNECTIONS -d$DURATION http://127.0.0.1:$PORT/ 2>&1)

    # Record RSS after benchmark
    local RSS_AFTER=$(ps -o rss= -p $PID 2>/dev/null | awk '{printf "%.1f", $1/1024}')

    # Parse results
    local RPS=$(echo "$RESULT" | grep "Requests/sec" | awk '{print $2}')
    local LATENCY=$(echo "$RESULT" | grep "Latency" | awk '{print $2}')
    local LATENCY_MAX=$(echo "$RESULT" | grep "Latency" | awk '{print $4}')
    local TOTAL=$(echo "$RESULT" | grep "requests in" | awk '{print $1}')
    local ERRORS=$(echo "$RESULT" | grep -i "non-2xx\|socket errors" || echo "0")
    local TRANSFER=$(echo "$RESULT" | grep "Transfer/sec" | awk '{print $2}')

    printf "    %-14s %10s req/s\n" "Throughput:" "$RPS"
    printf "    %-14s %10s avg / %s max\n" "Latency:" "$LATENCY" "$LATENCY_MAX"
    printf "    %-14s %10s\n" "Total reqs:" "$TOTAL"
    printf "    %-14s %10s\n" "Transfer:" "$TRANSFER"
    printf "    %-14s %10s MB -> %s MB\n" "Memory:" "$RSS_BEFORE" "$RSS_AFTER"
    if echo "$ERRORS" | grep -q "non-2xx\|Socket"; then
        printf "    %-14s %s\n" "Errors:" "$ERRORS"
    else
        printf "    %-14s %10s\n" "Errors:" "0"
    fi

    kill $PID 2>/dev/null || true
    wait $PID 2>/dev/null || true
    sleep 1
}

# --- Run all 4 configurations ---

echo ""
echo "============================================================"
echo "  Config 1: No Logging (level=OFF)"
echo "  Zero-cost baseline — tracing macros skip entirely"
echo "============================================================"
run_bench "OFF" /tmp/pyre_bench_log_off.py

echo ""
echo "============================================================"
echo "  Config 2: Server Log Only (level=ERROR)"
echo "  Default production — only errors reach the writer"
echo "============================================================"
run_bench "ERROR" /tmp/pyre_bench_log_error.py

echo ""
echo "============================================================"
echo "  Config 3: Access Log Enabled (level=INFO + access_log)"
echo "  Every request logged: method, path, status, latency_us"
echo "============================================================"
run_bench "ACCESS" /tmp/pyre_bench_log_access.py

echo ""
echo "============================================================"
echo "  Config 4: Access Log + User Logging (Python logging.info)"
echo "  Access log + one logging.info() call per request via FFI"
echo "============================================================"
run_bench "USER_LOG" /tmp/pyre_bench_log_user.py

echo ""
echo "============================================================"
echo "  Benchmark complete!"
echo "============================================================"
