"""G2 verdict (spec §6, §8.2): compare the Jev-off, Jev-on and Jev-on-without-
tools arms of the A/B set and decide the quality / efficiency paths and the
hard gates (latency added per step, Chinese subset).

python -m eval.harness.gate --off eval/results/g2-off --on eval/results/g2-on \
    [--on-notools eval/results/g2-notools] [--x 20] [--weights jev_input=0.2] \
    [--out eval/results/g2/report.md]

Exit status: 0 PASS, 1 FAIL, 2 INVALID.
"""

import argparse
import json
import pathlib
import statistics
import sys

from eval.harness.calibrate import (DEFAULT_WEIGHTS, effective_cost, percentile, step_waits,
                                    summarize)

RHO = 0.25          # ADR J11
LATENCY_MS = 250    # spec §6: P50 added per step, over all steps
ZH = "-zh-"


def _latest(rows):
    latest = {}
    for r in rows:
        latest[(r["rep"], r["task_id"])] = r
    return list(latest.values())


def _jev_activity(rows):
    calls = fallbacks = 0
    for r in rows:
        for u in r.get("usage") or []:
            j = u.get("jev") or {}
            calls += j.get("calls", 0)
            fallbacks += j.get("fallbacks", 0)
    return calls, fallbacks


def _rate(rows):
    return statistics.mean(1.0 if r["pass"] else 0.0 for r in rows) if rows else None


def _cost(rows, weights):
    return statistics.mean(effective_cost(r, weights) for r in rows) if rows else 0.0


def gate(off, on, notools=None, *, rho=RHO, x=None, latency_ms=LATENCY_MS,
         weights=DEFAULT_WEIGHTS):
    """The G2 verdict for trial rows of each arm (see the module docs)."""
    arms = {"off": _latest(off), "on": _latest(on)}
    if notools is not None:
        arms["notools"] = _latest(notools)
    problems, warnings = [], []

    tasks = {k: frozenset(r["task_id"] for r in v) for k, v in arms.items()}
    reps = {k: len({r["rep"] for r in v}) for k, v in arms.items()}
    if len(set(tasks.values())) > 1:
        problems.append(f"arms ran different task sets: {({k: len(t) for k, t in tasks.items()})}")
    if len(set(reps.values())) > 1:
        problems.append(f"arms ran different k: {reps}")

    sums = {k: summarize(v) for k, v in arms.items()}
    for k, s in sums.items():
        if s["missing_rate"] > 0.10:
            problems.append(f"{k}: {s['missing_rate']:.0%} of trials missing (> 10%)")
        if s["cost"]["estimated_share"] > 0.10:
            warnings.append(f"{k}: {s['cost']['estimated_share']:.0%} of turns have "
                            "estimated usage (low confidence)")
    for k in ("on", "notools"):
        if k in arms:
            calls, fb = _jev_activity(arms[k])
            if calls == 0 or fb > 0.5 * (calls + fb):
                problems.append(f"{k}: Jev inactive ({calls} answered, {fb} fell back)")

    # δ = 2·sd of the baseline's per-rep scores (R10), but never below one
    # task's flip: on a steady baseline a tie must not count as a gain.
    per_rep = sums["off"]["per_rep_score"]
    sd = statistics.stdev(per_rep) if len(per_rep) > 1 else 0.0
    delta = max(2 * sd, 1 / max(1, len(tasks["off"])))

    score = {"off": sums["off"]["mean"], "on": sums["on"]["mean"]}
    score["diff"] = score["on"] - score["off"]
    c_off, c_on = _cost(arms["off"], weights), _cost(arms["on"], weights)
    cost = {"off": c_off, "on": c_on, "change": (c_on / c_off - 1) if c_off else 0.0}

    waits = [ms for r in arms["on"] for ms in step_waits(r)]
    latency = {"steps": len(waits), "p50": percentile(waits, 50), "p90": percentile(waits, 90)}

    zh_off = _rate([r for r in arms["off"] if ZH in r["task_id"]])
    zh_on = _rate([r for r in arms["on"] if ZH in r["task_id"]])
    if zh_off is None or zh_on is None:
        warnings.append("no Chinese tasks: the Chinese-subset gate is not evaluated")
        regressed = False
    else:
        regressed = zh_on < zh_off - delta

    quality = score["diff"] >= delta and cost["change"] <= rho
    efficiency = None if x is None else (score["diff"] >= -delta and cost["change"] <= -x / 100)
    lat_ok = latency["p50"] is None or latency["p50"] <= latency_ms
    valid = not problems

    tools = None
    if "notools" in arms:
        c_b = _cost(arms["notools"], weights)
        diff = score["on"] - sums["notools"]["mean"]
        change = (c_on / c_b - 1) if c_b else 0.0
        # §8.2: no gain of δ and dearer → split tool assembly out.
        tools = {"score_diff": diff, "cost_change": change,
                 "decision": "split" if diff < delta and change > 0 else "keep"}

    return {
        "valid": valid, "problems": problems, "warnings": warnings, "delta": delta,
        "rho": rho, "x": x, "latency_ms": latency_ms,
        "score": score, "cost": cost, "latency": latency,
        "zh": {"off": zh_off, "on": zh_on, "regressed": regressed},
        "paths": {"quality": quality, "efficiency": efficiency},
        "hard_gates": {"latency": lat_ok, "zh": not regressed},
        "pass": valid and (quality or efficiency is True) and lat_ok and not regressed,
        "tools": tools,
    }


def _mark(ok):
    return "✓" if ok else "✗"


def _num(v, fmt="{:.3f}"):
    return "—" if v is None else fmt.format(v)


def render(v) -> str:
    """The verdict as a Markdown report."""
    title = "INVALID" if not v["valid"] else ("PASS" if v["pass"] else "FAIL")
    out = [f"# G2: {title}", ""]
    if v["problems"]:
        out += ["**Problems** (the verdict is not usable):", ""]
        out += [f"- {p}" for p in v["problems"]] + [""]
    if v["warnings"]:
        out += ["**Warnings:**", ""] + [f"- {w}" for w in v["warnings"]] + [""]
    s, c, lat, zh = v["score"], v["cost"], v["latency"], v["zh"]
    out += [
        "| | Jev off | Jev on | Change | Threshold |",
        "|---|---|---|---|---|",
        f"| Score | {_num(s['off'])} | {_num(s['on'])} | {s['diff']:+.3f} | δ = {v['delta']:.3f} |",
        f"| Effective cost / trial | {c['off']:,.0f} | {c['on']:,.0f} | {c['change']:+.1%} "
        f"| ρ = {v['rho']:.0%} |",
        f"| Jev wait per step (P50 / P90, {lat['steps']} steps) | — | "
        f"{_num(lat['p50'], '{:.0f}')} / {_num(lat['p90'], '{:.0f}')} ms | | "
        f"≤ {v['latency_ms']} ms {_mark(v['hard_gates']['latency'])} |",
        f"| Chinese subset | {_num(zh['off'])} | {_num(zh['on'])} | "
        f"{'regressed' if zh['regressed'] else 'no regression'} | {_mark(v['hard_gates']['zh'])} |",
        "",
        "**Paths:**",
        "",
        f"- quality (score ≥ +δ, cost ≤ +ρ): {_mark(v['paths']['quality'])}",
    ]
    eff = v["paths"]["efficiency"]
    out.append("- efficiency (score ≥ −δ, cost ≤ −X%): "
               + ("not evaluable (X not set)" if eff is None else f"{_mark(eff)} (X = {v['x']}%)"))
    if v["tools"]:
        t = v["tools"]
        out += ["", f"**§8.2 tool assembly:** {t['decision']} — score on − without tools "
                    f"{t['score_diff']:+.3f}, cost {t['cost_change']:+.1%}."]
    return "\n".join(out) + "\n"


def _read(d):
    path = pathlib.Path(d) / "trials.jsonl"
    if not path.is_file():
        raise SystemExit(f"no results: {path} does not exist")
    with open(path, encoding="utf-8") as f:
        return [json.loads(l) for l in f if l.strip()]


def _weights(text):
    w = dict(DEFAULT_WEIGHTS)
    for part in filter(None, (text or "").split(",")):
        k, _, val = part.partition("=")
        if k not in w:
            raise SystemExit(f"unknown weight {k!r}; known: {', '.join(w)}")
        w[k] = float(val)
    return w


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description="G2 verdict from the A/B arms' results")
    ap.add_argument("--off", required=True, help="Jev-off results directory")
    ap.add_argument("--on", required=True, help="Jev-on results directory")
    ap.add_argument("--on-notools", help="Jev-on, tools point off (§8.2)")
    ap.add_argument("--x", type=float, help="efficiency path: required cost cut, percent")
    ap.add_argument("--weights", help="effective cost weights, k=v,... over the defaults")
    ap.add_argument("--out", help="write the report here and the verdict to <out>.json")
    a = ap.parse_args(argv)
    v = gate(_read(a.off), _read(a.on), _read(a.on_notools) if a.on_notools else None,
             x=a.x, weights=_weights(a.weights))
    md = render(v)
    # UTF-8 bytes: the Windows console's cp1252 cannot encode δ / ρ / ✓.
    out_stream = getattr(sys.stdout, "buffer", None)
    if out_stream is not None:
        out_stream.write(md.encode("utf-8"))
        out_stream.flush()
    else:
        sys.stdout.write(md)
    if a.out:
        out = pathlib.Path(a.out)
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_bytes(md.encode("utf-8"))
        pathlib.Path(str(out) + ".json").write_bytes(
            json.dumps(v, ensure_ascii=False, indent=1).encode("utf-8"))
    return 2 if not v["valid"] else (0 if v["pass"] else 1)


if __name__ == "__main__":
    sys.exit(main())
