#!/usr/bin/env bash
# Time from launch until the health check (GET /) answers, averaged over N runs.
#   tools/bench/startup.sh [binary] [runs]
set -euo pipefail
BIN=${1:-target/release/datastore-oxide} RUNS=${2:-20} PORT=18190
total=0
for _ in $(seq "$RUNS"); do
  start=$(date +%s%N)
  "$BIN" start --testing --port=$PORT >/dev/null 2>&1 &
  pid=$!
  until curl -sf "http://127.0.0.1:$PORT/" >/dev/null; do :; done
  end=$(date +%s%N)
  kill "$pid"; wait "$pid" 2>/dev/null || true
  total=$((total + end - start))
done
echo "startup to health check: $((total / RUNS / 1000000)) ms (mean of $RUNS)"
