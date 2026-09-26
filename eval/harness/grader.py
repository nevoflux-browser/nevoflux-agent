"""Deterministic grading: answer regexes and the site's event log."""

import operator
import re

_OPS = {"==": operator.eq, "<=": operator.le, ">=": operator.ge}


def _text(result, turn):
    outs = result.get("turn_outputs") or []
    if outs:
        try:
            return outs[turn]
        except IndexError:
            return None
    return result.get("output") if turn in (-1, 0) else None


def _events(result):
    trial = result.get("trial")
    evs = result.get("events") or []
    return [e for e in evs if trial is None or e.get("trial") == trial]


def _check(c, result):
    t = c["type"]
    if t == "output_regex":
        text = _text(result, c.get("turn", -1))
        if not text:
            return False, "no output"
        ok = re.search(c["pattern"], text, re.I | re.S) is not None
        return ok, "matched" if ok else f"no match for {c['pattern']!r}"
    evs = [e for e in _events(result) if e.get("kind") == c["kind"]]
    if t == "event":
        where = c.get("where", {})
        ok = any(all(e.get("data", {}).get(k) == v for k, v in where.items()) for e in evs)
        return ok, f"{len(evs)} {c['kind']} events"
    if t == "event_count":
        ok = _OPS[c["op"]](len(evs), c["n"])
        return ok, f"{len(evs)} {c['kind']} events (want {c['op']} {c['n']})"
    if t == "no_event":
        return not evs, f"{len(evs)} {c['kind']} events"
    raise ValueError(f"unknown check type {t}")


def grade(spec, result) -> dict:
    rows = []
    for c in spec.checks:
        ok, why = _check(c, result)
        rows.append({"check": c, "ok": ok, "why": why})
    return {"pass": all(r["ok"] for r in rows), "checks": rows}
