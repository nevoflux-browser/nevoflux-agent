"""Batch runner: every task in a directory, k times, serially.

python -m eval.harness.run --tasks eval/harness/tasks/j20 --k 3 --out eval/results/j20-base
Re-running with the same --out resumes: trials that already have a real
result are skipped, missing ones (provider/harness errors) run again.
"""

import argparse
import datetime
import json
import os
import pathlib
import statistics
import sys

from .grader import MISSING
from .siteserver import SiteServer
from .taskspec import load_dir
from .trial import TrialConfig, run_trial

HERE = pathlib.Path(__file__).resolve().parent
DEFAULT_BROWSER = r"C:\Users\Docker\nevoflux\nevoflux\engine\obj-x86_64-pc-windows-msvc\dist\bin\nevoflux.exe"
STOP_AFTER = 3  # consecutive provider errors: the provider is down or out of quota


def _parse_set(items):
    out = {}
    for it in items or []:
        k, _, v = it.partition("=")
        low = v.lower()
        out[k] = True if low == "true" else False if low == "false" else int(v) if v.isdigit() else v
    return out


def latest_rows(rows):
    """One row per (rep, task): the last one written wins."""
    latest = {}
    for r in rows:
        latest[(r["rep"], r["task_id"])] = r
    return list(latest.values())


def plan_trials(specs, k, done_rows):
    """(rep, spec) pairs still to run: those without a real (non-missing) result."""
    have = {(r["rep"], r["task_id"]) for r in latest_rows(done_rows) if r["status"] not in MISSING}
    return [(rep, s) for rep in range(k) for s in specs if (rep, s.id) not in have]


def should_stop(statuses, n=STOP_AFTER):
    tail = statuses[-n:]
    return len(tail) == n and all(s == "provider_error" for s in tail)


def _summary(rows, specs, k):
    rows = latest_rows(rows)
    ids = [s.id for s in specs]
    per_task = {i: statistics.mean(r["pass"] for r in rows if r["task_id"] == i)
                for i in ids if any(r["task_id"] == i for r in rows)}
    per_rep = [statistics.mean(r["pass"] for r in rows if r["rep"] == rep)
               for rep in range(k) if any(r["rep"] == rep for r in rows)]
    return {"score": statistics.mean(per_task.values()) if per_task else 0.0,
            "per_rep": per_rep, "per_task": per_task,
            "trials": len(rows), "expected_trials": len(ids) * k,
            "harness_errors": sum(r["status"] == "harness_error" for r in rows),
            "timeouts": sum(r.get("timed_out", False) for r in rows),
            "provider_errors": sum(r["status"] == "provider_error" for r in rows)}


def main(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument("--tasks", required=True)
    ap.add_argument("--k", type=int, default=3)
    ap.add_argument("--out", required=True)
    ap.add_argument("--set", action="append", help="config override, e.g. llm.provider=anthropic")
    ap.add_argument("--only", default="")
    ap.add_argument("--agent-exe", default=str(HERE.parents[1] / "target" / "release" / "nevoflux-agent.exe"))
    ap.add_argument("--browser-bin", default=os.environ.get("NEVOFLUX_BROWSER_BIN", DEFAULT_BROWSER))
    ap.add_argument("--base-config", default=str(pathlib.Path(os.environ["APPDATA"]) / "nevoflux" / "config.toml"))
    ap.add_argument("--keep-dirs", action="store_true")
    a = ap.parse_args(argv)

    specs = load_dir(a.tasks)
    if a.only:
        wanted = set(a.only.split(","))
        specs = [s for s in specs if s.id in wanted]
    out = pathlib.Path(a.out)
    out.mkdir(parents=True, exist_ok=True)
    trials_file = out / "trials.jsonl"
    done = ([json.loads(l) for l in trials_file.read_text(encoding="utf-8").splitlines() if l.strip()]
            if trials_file.exists() else [])
    todo = plan_trials(specs, a.k, done)
    cfg = TrialConfig(agent_exe=pathlib.Path(a.agent_exe), browser_bin=pathlib.Path(a.browser_bin),
                      base_config=pathlib.Path(a.base_config), overrides=_parse_set(a.set),
                      work_root=out / "trials", keep_dirs=a.keep_dirs)
    with (out / "run.json").open("a", encoding="utf-8") as f:
        f.write(json.dumps({
            "started": datetime.datetime.now().isoformat(), "tasks": a.tasks, "k": a.k,
            "overrides": cfg.overrides, "agent_exe": a.agent_exe, "browser_bin": a.browser_bin,
            "resumed_from": len(done), "to_run": len(todo),
        }, ensure_ascii=False) + "\n")
    print(f"{len(todo)} trials to run ({len(done)} rows already in {trials_file})", flush=True)

    site = SiteServer(HERE / "sites")
    site.start()
    rows, statuses, stopped = list(done), [], False
    try:
        with trials_file.open("a", encoding="utf-8") as f:
            for rep, s in todo:
                row = run_trial(s, site, cfg, f"r{rep}-{s.id}")
                row["rep"] = rep
                rows.append(row)
                statuses.append(row["status"])
                f.write(json.dumps(row, ensure_ascii=False) + "\n")
                f.flush()
                print(f"[{rep}] {s.id:28} {'PASS' if row['pass'] else 'fail'} "
                      f"{row['status']:14} {row['secs']:6.0f}s", flush=True)
                if should_stop(statuses):
                    stopped = True
                    print(f"stopping: {STOP_AFTER} provider errors in a row "
                          f"(provider down or out of quota); re-run the same command to resume",
                          flush=True)
                    break
    finally:
        site.stop()

    summary = _summary(rows, specs, a.k)
    summary["stopped_early"] = stopped
    (out / "summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=1), encoding="utf-8")
    print(json.dumps(summary, ensure_ascii=False, indent=1))
    return 2 if stopped else 0


if __name__ == "__main__":
    sys.exit(main())
