#!/usr/bin/env bash
# Tear down the reused session browser out-of-band (session mode).
# Usage:  ./session-close.sh
set -uo pipefail
HOST="${HOST:-http://localhost:8080}"
echo "POST $HOST/session/close"
curl -s -m 10 -X POST "$HOST/session/close"; echo
CONTAINER="$(docker compose ps --format '{{.Name}}' 2>/dev/null | head -1)"; CONTAINER="${CONTAINER:-headless-headless-1}"
sleep 2
echo "browser procs now: $(docker top "$CONTAINER" 2>/dev/null | grep -cE '/opt/nevoflux/nevoflux')  (0 = torn down)"
