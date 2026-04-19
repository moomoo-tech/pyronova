#!/usr/bin/env bash
set -e

cd "$(dirname "$0")/.."
source .venv/bin/activate

cleanup() {
    lsof -ti:8000 | xargs kill -9 2>/dev/null || true
    lsof -ti:8001 | xargs kill -9 2>/dev/null || true
}
trap cleanup EXIT
cleanup 2>/dev/null

echo "============================================================"
echo "  Memory Benchmark: SkyTrade Engine vs Robyn"
echo "============================================================"

# --- Helper: sample RSS over time ---
sample_rss() {
    local pid=$1
    local label=$2
    local duration=$3
    local max_rss=0
    local start_rss=0
    local end_rss=0
    local i=0

    # Wait for process to stabilize
    sleep 1
    start_rss=$(ps -o rss= -p "$pid" 2>/dev/null | tr -d ' ')
    [ -z "$start_rss" ] && start_rss=0

    echo "  [${label}] PID=${pid}, idle RSS: $((start_rss / 1024)) MB"

    # Run wrk load in background
    wrk -t4 -c256 -d${duration}s http://127.0.0.1:$4/ > /dev/null 2>&1 &
    local wrk_pid=$!

    # Sample RSS every 0.5s during load
    while kill -0 $wrk_pid 2>/dev/null; do
        local rss=$(ps -o rss= -p "$pid" 2>/dev/null | tr -d ' ')
        [ -z "$rss" ] && break
        [ "$rss" -gt "$max_rss" ] && max_rss=$rss
        sleep 0.5
    done
    wait $wrk_pid 2>/dev/null || true

    end_rss=$(ps -o rss= -p "$pid" 2>/dev/null | tr -d ' ')
    [ -z "$end_rss" ] && end_rss=0

    echo "  [${label}] peak RSS under load: $((max_rss / 1024)) MB"
    echo "  [${label}] post-load RSS: $((end_rss / 1024)) MB"
    echo ""
}

# --- SkyTrade ---
echo ""
echo ">>> Starting SkyTrade Engine on :8000 ..."
python benchmarks/bench_plaintext.py &
SKY_PID=$!
sleep 2

sample_rss $SKY_PID "SkyTrade" 10 8000

kill $SKY_PID 2>/dev/null || true
sleep 1

# --- Robyn ---
echo ">>> Starting Robyn on :8001 ..."
python benchmarks/robyn_app.py &
ROBYN_PID=$!
sleep 3

sample_rss $ROBYN_PID "Robyn" 10 8001

kill $ROBYN_PID 2>/dev/null || true

echo "============================================================"
echo "  Memory benchmark complete!"
echo "============================================================"
