"""δ calibration from k baseline passes (δ = 2·sd of per-pass scores).

python -m eval.harness.calibrate eval/results/baseline-j20/trials.jsonl
"""

import json
import statistics
import sys

MISSING = {"harness_error", "timeout", "provider_error"}


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
    cache_read = sum(b.get("cache_read") or 0 for b in buckets)
    return {
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
