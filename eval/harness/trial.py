"""Run one eval task once, end to end, in its own directory."""

import dataclasses
import pathlib
import re
import shutil
import tempfile
import time

from . import daemon as dmod
from .grader import grade
from .taskspec import render
from .tomledit import override


@dataclasses.dataclass
class TrialConfig:
    agent_exe: pathlib.Path
    browser_bin: pathlib.Path
    base_config: pathlib.Path
    overrides: dict
    work_root: pathlib.Path
    keep_dirs: bool = False


_PROVIDER_ERROR = re.compile(r"^\s*\[Error: ProviderError")


def classify_status(last: dict, timed_out: bool) -> str:
    """Trial status. A provider failure (the daemon reports it as a
    'succeeded' task whose output is the error text) is not an agent result:
    it is a missing trial, counted against eval validity, not against the
    agent."""
    if timed_out:
        return "timeout"
    texts = [last.get("output") or ""] + list(last.get("turn_outputs") or [])
    if any(_PROVIDER_ERROR.match(t) for t in texts):
        return "provider_error"
    return last.get("status") or "unknown"


def check_effective(overrides: dict, eff: dict):
    """Error text if the daemon did not load what the trial asked for."""
    if eff.get("load_error"):
        return f"daemon fell back to default config: {eff['load_error']}"
    want = overrides.get("llm.provider")
    if want is not None and eff.get("provider") != want:
        return f"asked for llm.provider={want}, daemon loaded {eff.get('provider')}"
    return None


def _delays(spec) -> int:
    return sum(int(f.get("delay_secs", 0)) for f in spec.followups)


def build_task_body(spec) -> dict:
    return {
        "task": spec.task,
        "mode": spec.mode,
        "followups": [{"message": f["message"], "delay_secs": int(f.get("delay_secs", 0))}
                      for f in spec.followups],
        "no_retry": True,
        "wall_clock_secs": spec.timeout_secs + _delays(spec),
    }


def run_trial(spec, site, cfg: TrialConfig, trial_id: str) -> dict:
    cfg.work_root.mkdir(parents=True, exist_ok=True)
    tdir = pathlib.Path(tempfile.mkdtemp(prefix=f"{trial_id}-", dir=cfg.work_root))
    text = cfg.base_config.read_text(encoding="utf-8")
    for k, v in cfg.overrides.items():
        text = override(text, k, v)
    (tdir / "config.toml").write_text(text, encoding="utf-8")

    site.set_trial(trial_id)
    rendered = render(spec, f"http://127.0.0.1:{site.port}/{spec.site}")
    d = dmod.Daemon(cfg.agent_exe, tdir, cfg.browser_bin, dmod.free_port())
    keep = cfg.keep_dirs
    t0 = time.monotonic()
    row = {"trial": trial_id, "task_id": spec.id, "set": spec.set, "lang": spec.lang}
    try:
        d.start()
        eff = d.effective_config()
        row.update({"provider": eff["provider"], "model": eff["model"]})
        problem = check_effective(cfg.overrides, eff)
        if problem:
            raise RuntimeError(problem)
        tid = d.submit(build_task_body(rendered))
        last, timed_out = dmod.poll(lambda: d.get(tid),
                                    timeout_secs=spec.timeout_secs + _delays(spec) + 60)
        if timed_out:
            d.cancel(tid)
        sid = last.get("session_id")
        jsonl = tdir / "session.jsonl"
        exported = bool(sid) and d.export_session(sid, jsonl)
        result = {"status": classify_status(last, timed_out),
                  "turn_outputs": last.get("turn_outputs") or [],
                  "output": last.get("output"), "events": site.events(), "trial": trial_id}
        g = grade(spec, result)
        row.update({
            "status": result["status"], "timed_out": timed_out, "pass": g["pass"],
            "checks": g["checks"], "turn_outputs": result["turn_outputs"],
            "output": result["output"], "error": last.get("error"),
            "usage": last.get("usage") or [], "session_id": sid,
            "events": result["events"],
            "session_jsonl": (jsonl.read_text(encoding="utf-8") if exported else None),
        })
    except Exception as e:  # a broken trial scores 0 and keeps its directory
        row.update({"status": "harness_error", "timed_out": False, "pass": False,
                    "checks": [], "error": f"{type(e).__name__}: {e}", "usage": []})
        keep = True
    finally:
        d.stop()
        row["secs"] = round(time.monotonic() - t0, 1)
        row["dir"] = str(tdir)
        if not keep:
            shutil.rmtree(tdir, ignore_errors=True)
    return row
