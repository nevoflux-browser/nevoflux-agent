"""δ calibration from k baseline passes (δ = 2·sd of per-pass scores).

python -m eval.harness.calibrate eval/results/baseline-j20/trials.jsonl
"""

import json
import math
import statistics
import sys

MISSING = {"harness_error", "timeout", "provider_error"}


# Effective cost C (v1.4 §10.4) in equivalent input tokens: the Anthropic
# wire's cache weights (`economics::cache_rate`), output at 5x, and Jev's
# tokens priced like the main model's (TypeSafe's price is unknown; this
# counts against Jev).
DEFAULT_WEIGHTS = {"input": 1.0, "cache_write": 1.25, "cache_read": 0.1, "output": 5.0,
                   "jev_input": 1.0, "jev_output": 5.0}


def effective_cost(row, weights=DEFAULT_WEIGHTS) -> float:
    """Effective cost C of one trial (v1.4 §10.4), in equivalent input tokens:
    uncached input, cache writes and reads, output, and Jev's tokens."""
    w = weights
    c = 0.0
    for u in row.get("usage") or []:
        for b in (u.get("main"), u.get("subagent")):
            if not b:
                continue
            read = b.get("cache_read") or 0
            write = b.get("cache_write") or 0
            fresh = max(0, b.get("input", 0) - read - write)
            c += (fresh * w["input"] + write * w["cache_write"] + read * w["cache_read"]
                  + b.get("output", 0) * w["output"])
        j = u.get("jev") or {}
        c += j.get("input", 0) * w["jev_input"] + j.get("output", 0) * w["jev_output"]
    return c


def step_waits(row) -> list:
    """Milliseconds the loop blocked on Jev (`jev/wait`), per step (spec §6:
    over all steps, a quiet step is 0). A wait goes to the step it happened
    in; one before a turn's first step (the turn-start choice) goes to that
    step; one with no step after it counts as a step of its own."""
    steps, cur, pending = [], None, 0
    for line in (row.get("session_jsonl") or "").splitlines():
        if not line.strip():
            continue
        e = json.loads(line)
        t = e.get("type")
        if t == "turn/start":
            cur = None
        elif t == "step/start":
            steps.append(pending)
            pending = 0
            cur = len(steps) - 1
        elif t == "jev/wait":
            if cur is None:
                pending += e.get("ms", 0)
            else:
                steps[cur] += e.get("ms", 0)
    if pending:
        steps.append(pending)
    return steps


def percentile(values, q):
    """Nearest-rank percentile (q in 0..100); None for no values."""
    if not values:
        return None
    v = sorted(values)
    k = max(0, min(len(v) - 1, math.ceil(q / 100 * len(v)) - 1))
    return v[k]


def jev_summary(rows) -> dict:
    """Jev numbers from the session logs (spec §8.2, §8.3): grades, fallbacks,
    per-step signals and the error of H, corrections, recalls, and the tool
    sets (decisions by reason, missed tools, tool_change rebuilds, the size of
    a turn-start set and how long choosing it took), the skills Jev loaded
    with how often the model loaded a different one in the same turn, and the
    calls Jev flagged as risky (spec §5.7, J14) with how many of those nobody
    could confirm.

    H is "tool-using steps still needed" asked at step s; the actual value is
    last step of the turn - s - 1 (the last step is the answer).
    """
    vis, corrections = {}, {}
    fallbacks = signals = recalls = 0
    errors = []
    tool_sets, set_sizes, select_ms = {}, [], []
    missed = tool_change_rebuilds = 0
    # Skills Jev loaded (spec §5.7), and turns where the model then loaded a
    # different skill itself: the likely misinjections (§8.3).
    injected = other_loaded = 0
    flagged = flagged_unattended = 0
    for r in rows:
        events = [json.loads(l) for l in (r.get("session_jsonl") or "").splitlines() if l.strip()]
        turn, last_step, asked = None, {}, []
        # skills/inject is logged at turn start, before the agent's
        # turn/start; it lasts until that turn's turn/end. A subagent's turn
        # nested inside (turn/start while one is open) is not a turn.
        injected_now, other_now, depth = None, False, 0
        for e in events:
            t = e.get("type")
            if t == "turn/start":
                depth += 1
            elif t == "turn/end":
                depth = max(0, depth - 1)
                if depth == 0:
                    if injected_now and other_now:
                        other_loaded += 1
                    injected_now, other_now = None, False
            if t == "turn/start":
                turn = e.get("turn")
            elif t == "step/start":
                last_step[turn] = max(last_step.get(turn, 0), e.get("step", 0))
            elif t == "jev/visibility":
                k = f"{e.get('level')}/{e.get('graded_by')}"
                vis[k] = vis.get(k, 0) + 1
            elif t == "jev/fallback":
                fallbacks += 1
            elif t == "jev/signals":
                signals += 1
                if e.get("h") is not None:
                    asked.append((turn, e["step"], e["h"]))
            elif t == "context/correction":
                for trig in e.get("triggers") or []:
                    corrections[trig] = corrections.get(trig, 0) + 1
            elif t == "tool/call" and e.get("name") == "recall":
                recalls += 1
            elif t == "tools/select":
                reason = e.get("reason")
                tool_sets[reason] = tool_sets.get(reason, 0) + 1
                if reason in ("missed", "act"):
                    missed += 1
                else:
                    set_sizes.append(len(e.get("names") or []))
                    if e.get("elapsed_ms"):
                        select_ms.append(e["elapsed_ms"])
            elif t == "context/rebuild" and e.get("reason") == "tool_change":
                tool_change_rebuilds += 1
            elif t == "skills/inject":
                injected += 1
                injected_now = e.get("name")
            elif t == "permission/jev":
                flagged += 1
                if e.get("unattended"):
                    flagged_unattended += 1
            if (t == "tool/call" and e.get("name") == "skill_load" and injected_now
                    and (e.get("args") or {}).get("name") != injected_now):
                other_now = True
        for turn_, step, h in asked:
            if turn_ in last_step:
                errors.append(h - max(0, last_step[turn_] - step - 1))
    return {
        "visibility": vis,
        "fallbacks": fallbacks,
        "signals": signals,
        "h_mae": statistics.mean(abs(x) for x in errors) if errors else None,
        "h_bias": statistics.mean(errors) if errors else None,
        "corrections": corrections,
        "recalls": recalls,
        "tool_sets": tool_sets,
        "missed": missed,
        "tool_change_rebuilds": tool_change_rebuilds,
        "tool_set_size": statistics.mean(set_sizes) if set_sizes else None,
        "tool_select_ms": statistics.mean(select_ms) if select_ms else None,
        "skills": {"injected": injected, "other_loaded": other_loaded},
        "permissions": {"flagged": flagged, "unattended": flagged_unattended},
    }


def summarize(rows) -> dict:
    # A resumed run re-runs missing trials; only the latest row per trial counts.
    latest = {}
    for r in rows:
        latest[(r["rep"], r["task_id"])] = r
    rows = list(latest.values())
    reps = sorted({r["rep"] for r in rows})
    per_rep = [statistics.mean(1.0 if r["pass"] else 0.0 for r in rows if r["rep"] == k) for k in reps]
    ids = sorted({r["task_id"] for r in rows})
    rate = {i: statistics.mean(1.0 if r["pass"] else 0.0 for r in rows if r["task_id"] == i) for i in ids}
    sd = statistics.stdev(per_rep) if len(per_rep) > 1 else 0.0
    turns = [u for r in rows for u in (r.get("usage") or [])]
    buckets = [b for u in turns for b in (u.get("main"), u.get("subagent")) if b]
    missing = sum(r["status"] in MISSING for r in rows) / len(rows) if rows else 0.0
    total_input = sum(b.get("input", 0) for b in buckets)
    costs = [effective_cost(r) for r in rows]
    waits = [ms for r in rows for ms in step_waits(r)]
    cache_read = sum(b.get("cache_read") or 0 for b in buckets)
    return {
        "jev": jev_summary(rows),
        "effective_cost": {"per_trial": statistics.mean(costs) if costs else 0.0,
                           "total": sum(costs)},
        "jev_wait": {"steps": len(waits), "p50": percentile(waits, 50),
                     "p90": percentile(waits, 90),
                     "mean": statistics.mean(waits) if waits else None},
        "per_rep_score": per_rep,
        "mean": statistics.mean(per_rep) if per_rep else 0.0,
        "sd": sd,
        "delta": 2 * sd,
        "per_task_pass_rate": rate,
        "flaky_tasks": [i for i, p in rate.items() if 0.0 < p < 1.0],
        "missing_rate": missing,
        "eval_invalid": missing > 0.10,
        "cost": {
            "input": total_input,
            "output": sum(b.get("output", 0) for b in buckets),
            # Cache reads/writes are part of `input`; None-reporting calls add 0.
            "cache_read": cache_read,
            "cache_write": sum(b.get("cache_write") or 0 for b in buckets),
            "cache_hit_ratio": cache_read / total_input if total_input else 0.0,
            # Jev is billed by TypeSafe, separately from the LLM.
            "jev_input": sum((u.get("jev") or {}).get("input", 0) for u in turns),
            "jev_output": sum((u.get("jev") or {}).get("output", 0) for u in turns),
            "jev_calls": sum((u.get("jev") or {}).get("calls", 0) for u in turns),
            "estimated_share": (sum(bool(u.get("main", {}).get("estimated")) for u in turns) / len(turns)
                                if turns else 0.0),
        },
    }


def main(path):
    with open(path, encoding="utf-8") as f:
        rows = [json.loads(l) for l in f if l.strip()]
    print(json.dumps(summarize(rows), ensure_ascii=False, indent=1))


if __name__ == "__main__":
    main(sys.argv[1])
