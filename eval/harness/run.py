"""Batch runner: every task in a directory, k times, serially.

python -m eval.harness.run --tasks eval/harness/tasks/j20 --k 3 --out eval/results/j20-base
"""

import argparse
import datetime
import json
import os
import pathlib
import statistics
import sys

from .siteserver import SiteServer
from .taskspec import load_dir
from .trial import TrialConfig, run_trial

HERE = pathlib.Path(__file__).resolve().parent
DEFAULT_BROWSER = r"C:\Users\Docker\nevoflux\nevoflux\engine\obj-x86_64-pc-windows-msvc\dist\bin\nevoflux.exe"


def _parse_set(items):
    out = {}
    for it in items or []:
        k, _, v = it.partition("=")
        low = v.lower()
        out[k] = True if low == "true" else False if low == "false" else int(v) if v.isdigit() else v
    return out


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
    cfg = TrialConfig(agent_exe=pathlib.Path(a.agent_exe), browser_bin=pathlib.Path(a.browser_bin),
                      base_config=pathlib.Path(a.base_config), overrides=_parse_set(a.set),
                      work_root=out / "trials", keep_dirs=a.keep_dirs)
    (out / "run.json").write_text(json.dumps({
        "started": datetime.datetime.now().isoformat(), "tasks": a.tasks, "k": a.k,
        "overrides": cfg.overrides, "agent_exe": a.agent_exe, "browser_bin": a.browser_bin,
    }, ensure_ascii=False, indent=1), encoding="utf-8")

    site = SiteServer(HERE / "sites")
    site.start()
    rows = []
    try:
        with (out / "trials.jsonl").open("a", encoding="utf-8") as f:
            for rep in range(a.k):
                for s in specs:
                    tid = f"r{rep}-{s.id}"
                    row = run_trial(s, site, cfg, tid)
                    row["rep"] = rep
                    rows.append(row)
                    f.write(json.dumps(row, ensure_ascii=False) + "\n")
                    f.flush()
                    print(f"[{rep}] {s.id:28} {'PASS' if row['pass'] else 'fail'} "
                          f"{row['status']:10} {row['secs']:6.0f}s", flush=True)
    finally:
        site.stop()

    per_task = {s.id: statistics.mean(r["pass"] for r in rows if r["task_id"] == s.id) for s in specs}
    per_rep = [statistics.mean(r["pass"] for r in rows if r["rep"] == k) for k in range(a.k)]
    summary = {"score": statistics.mean(per_task.values()) if per_task else 0.0,
               "per_rep": per_rep, "per_task": per_task,
               "harness_errors": sum(r["status"] == "harness_error" for r in rows),
               "timeouts": sum(r.get("timed_out", False) for r in rows)}
    (out / "summary.json").write_text(json.dumps(summary, ensure_ascii=False, indent=1), encoding="utf-8")
    print(json.dumps(summary, ensure_ascii=False, indent=1))
    return 0


if __name__ == "__main__":
    sys.exit(main())
