"""Pick the jev2 set from a Jev-off pilot of the candidates.

python -m eval.harness.select_tasks --pilot eval/results/jev2-pilot --out eval/harness/tasks/jev2-uncalibrated

The rule is fixed before the pilot runs: discriminating tasks first (they
sometimes pass, sometimes fail), at most three always-fail tasks (never an
infrastructure failure), then always-pass tasks — Chinese while Chinese is
under a third, multi-turn while multi-turn is under a third — until n. The
selection must land the pilot mean in the band, or there is none.
"""

import argparse
import json
import pathlib
import sys

from eval.harness.calibrate import MISSING

HERE = pathlib.Path(__file__).resolve().parent
CANDIDATES = HERE / "tasks" / "jev2-candidates"
MAX_ALWAYS_FAIL = 3


def select(rates, meta, n=30, band=(0.70, 0.85)):
    ids = sorted(i for i in rates if not meta[i]["infra_fail"])
    mixed = [i for i in ids if 0.0 < rates[i] < 1.0]
    fail = sorted((i for i in ids if rates[i] == 0.0), key=lambda i: (not meta[i]["multi"], i))
    easy = [i for i in ids if rates[i] == 1.0]
    picked = list(mixed[:n])
    picked += fail[:min(MAX_ALWAYS_FAIL, n - len(picked))]

    def share(key):
        return sum(1 for i in picked if key(i)) * 3

    while len(picked) < n and easy:
        if share(lambda i: meta[i]["lang"] == "zh") < len(picked) + 1:
            pool = [i for i in easy if meta[i]["lang"] == "zh"] or easy
        elif share(lambda i: meta[i]["multi"]) < len(picked) + 1:
            pool = [i for i in easy if meta[i]["multi"]] or easy
        else:
            pool = easy
        nxt = pool[0]
        picked.append(nxt)
        easy.remove(nxt)
    if len(picked) < n:
        return None

    def mean():
        return sum(rates[i] for i in picked) / n

    # Too hard: swap the hardest picks (always-fail first, then the lowest
    # rates) for always-pass tasks of the same language and turn shape.
    while mean() < band[0] and easy:
        out = min(picked, key=lambda i: (rates[i], i))
        if rates[out] == 1.0:
            break
        same = [i for i in easy if meta[i]["lang"] == meta[out]["lang"] and meta[i]["multi"] == meta[out]["multi"]]
        swap = (same or easy)[0]
        picked[picked.index(out)] = swap
        easy.remove(swap)
    if not band[0] <= mean() <= band[1]:
        return None
    return sorted(picked)


def pilot_rates(trials_path):
    latest = {}
    for line in pathlib.Path(trials_path).read_text(encoding="utf-8").splitlines():
        if line.strip():
            r = json.loads(line)
            latest[(r["rep"], r["task_id"])] = r
    by_task = {}
    for r in latest.values():
        by_task.setdefault(r["task_id"], []).append(r)
    rates, infra = {}, {}
    for tid, rows in by_task.items():
        real = [r for r in rows if r["status"] not in MISSING]
        infra[tid] = not real
        rates[tid] = sum(1.0 for r in real if r["pass"]) / len(real) if real else 0.0
    return rates, infra


def main(argv=None):
    ap = argparse.ArgumentParser()
    ap.add_argument("--pilot", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--n", type=int, default=30)
    a = ap.parse_args(argv)
    rates, infra = pilot_rates(pathlib.Path(a.pilot) / "trials.jsonl")
    cands = {p.stem: json.loads(p.read_text(encoding="utf-8")) for p in CANDIDATES.glob("*.json")}
    missing = sorted(set(cands) - set(rates))
    if missing:
        sys.exit(f"the pilot has no result for {len(missing)} candidates, e.g. {missing[:3]}")
    meta = {i: {"lang": t["lang"], "multi": bool(t["followups"]), "long": "long" in t["tags"],
                "infra_fail": infra[i]} for i, t in cands.items()}
    picked = select(rates, meta, n=a.n)
    mean_all = sum(rates.values()) / len(rates)
    if picked is None:
        sys.exit(f"no selection of {a.n} lands in the band (pilot mean over all candidates {mean_all:.3f}); "
                 "change one difficulty knob and re-pilot")
    out = pathlib.Path(a.out)
    for old in out.glob("*.json"):
        old.unlink()
    for i in picked:
        t = dict(cands[i], set=out.name)
        (out / f"{i}.json").write_bytes((json.dumps(t, ensure_ascii=False, indent=1) + "\n").encode("utf-8"))
    mean = sum(rates[i] for i in picked) / len(picked)
    print(f"picked {len(picked)}; pilot mean {mean:.3f} (all candidates {mean_all:.3f})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
