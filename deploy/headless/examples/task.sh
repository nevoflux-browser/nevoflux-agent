#!/usr/bin/env bash
# Submit ONE browser task and wait for its result.
# Usage:  ./task.sh "打开 https://www.google.com 搜索 nevoflux 并报告第一个结果"
#         ./task.sh "..." --end        # tear the session down after this task
set -uo pipefail
HOST="${HOST:-http://localhost:8080}"
TASK="${1:?usage: ./task.sh \"<instruction>\" [--end]}"
END=false; [ "${2:-}" = "--end" ] && END=true

body=$(printf '{"task":%s,"mode":"browser","end_session":%s,"wall_clock_secs":240}' \
        "$(printf '%s' "$TASK" | python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))' 2>/dev/null || printf '"%s"' "$TASK")" \
        "$END")
id=$(curl -s -m 10 "$HOST/tasks" -H 'Content-Type: application/json' -d "$body" | grep -oE 'task-[0-9]+' | head -1)
echo "submitted: $id  (end_session=$END)"
for i in $(seq 1 60); do
  r=$(curl -s -m 6 "$HOST/tasks/$id" 2>/dev/null)
  st=$(printf '%s' "$r" | grep -oE '"status":"[a-z]+"' | head -1 | cut -d'"' -f4)
  printf '\r[%3ds] %s        ' "$((i*4))" "${st:-?}"
  case "$st" in succeeded|failed) echo; echo "$r"; break;; esac
  sleep 4
done
