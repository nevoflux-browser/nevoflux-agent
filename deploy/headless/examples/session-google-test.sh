#!/usr/bin/env bash
# Session-mode E2E demo: a task-flow that reuses ONE warm browser across several
# Google searches for "nevoflux", then tears the session down.
#
# Prereqs: container up with NEVOFLUX_SESSION_MODE=1 (see docker-compose.yml).
# Watch it live:  http://localhost:6080/vnc.html   (VNC password: nevoflux)
#
# Usage:  ./session-google-test.sh
set -uo pipefail

HOST="${HOST:-http://localhost:8080}"
CONTAINER="${CONTAINER:-$(docker compose ps --format '{{.Name}}' 2>/dev/null | head -1)}"
CONTAINER="${CONTAINER:-headless-headless-1}"

# --- helpers ---------------------------------------------------------------
submit() {  # $1 = JSON body -> prints the task id
  curl -s -m 10 "$HOST/tasks" -H 'Content-Type: application/json' -d "$1" \
    | grep -oE 'task-[0-9]+' | head -1
}

wait_task() {  # $1 = id -> polls to terminal, prints status + output
  local id="$1" i st body
  for i in $(seq 1 60); do
    body="$(curl -s -m 6 "$HOST/tasks/$id" 2>/dev/null)"
    st="$(printf '%s' "$body" | grep -oE '"status":"[a-z]+"' | head -1 | cut -d'"' -f4)"
    printf '\r    [%3ds] %s        ' "$((i*4))" "${st:-?}"
    case "$st" in succeeded|failed) echo; break;; esac
    sleep 4
  done
  echo "    output: $(printf '%s' "$body" | grep -oE '"output":"([^"\\]|\\.)*"' | head -1 | sed -E 's/^"output":"//; s/"$//' | head -c 400)"
}

browser_procs() { docker top "$CONTAINER" 2>/dev/null | grep -cE '/opt/nevoflux/nevoflux'; }
browser_pid()   { docker top "$CONTAINER" 2>/dev/null | grep -E '/opt/nevoflux/nevoflux -no-remote' | awk '{print $2}' | head -1; }

step() { echo; echo "=================================================================="; echo "$1"; echo "=================================================================="; }

# --- flow ------------------------------------------------------------------
echo "container=$CONTAINER   host=$HOST"
echo "watch live: http://localhost:6080/vnc.html  (VNC password: nevoflux)"
echo "NOTE: Google may show a consent/anti-bot page; the agent tries to handle it."
echo "      If Google is blocked, edit the tasks to use https://duckduckgo.com instead."

step "TASK 1  — open Google, search \"nevoflux\", report the top result (LAUNCHES the session browser)"
ID=$(submit '{"task":"打开 https://www.google.com ，在搜索框输入 nevoflux 并回车搜索，报告第一个搜索结果的标题和网址","mode":"browser","wall_clock_secs":240}')
echo "  submitted: $ID"; wait_task "$ID"
sleep 2
PID1=$(browser_pid); echo "  >> browser procs after task 1: $(browser_procs)  (session mode keeps it alive; main pid=$PID1)"

step "TASK 2  — another Google search, REUSING the same warm browser"
ID=$(submit '{"task":"在 Google 搜索 \"nevoflux browser github\"，报告前 3 个搜索结果的标题","mode":"browser","wall_clock_secs":240}')
echo "  submitted: $ID"; wait_task "$ID"
sleep 2
PID2=$(browser_pid)
if [ -n "$PID1" ] && [ "$PID1" = "$PID2" ]; then echo "  >> REUSED the same browser (pid $PID2 unchanged) ✓"; else echo "  >> pid changed ($PID1 -> $PID2)"; fi

step "TASK 3  — final search WITH end_session:true (runs, then TEARS DOWN the browser)"
ID=$(submit '{"task":"在 Google 搜索 \"nevoflux agent\"，报告最相关结果的网址","mode":"browser","end_session":true,"wall_clock_secs":240}')
echo "  submitted: $ID"; wait_task "$ID"
sleep 3
echo "  >> browser procs after end_session: $(browser_procs)  (should be 0 = torn down) ✓"

echo; echo "done."
