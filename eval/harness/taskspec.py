"""Eval task specs: one JSON file per task."""

import dataclasses
import json
import pathlib

CHECK_TYPES = {"output_regex", "event", "event_count", "no_event"}


@dataclasses.dataclass
class TaskSpec:
    id: str
    set: str
    lang: str
    site: str
    task: str
    followups: list
    checks: list
    tags: list
    mode: str = "browser"
    timeout_secs: int = 300


def _validate(d: dict, path) -> None:
    for k in ("id", "set", "lang", "site", "task", "checks"):
        if k not in d:
            raise ValueError(f"{path}: missing {k}")
    if not d["checks"]:
        raise ValueError(f"{path}: checks must not be empty")
    for c in d["checks"]:
        if c.get("type") not in CHECK_TYPES:
            raise ValueError(f"{path}: bad check type {c.get('type')!r}")
    for f in d.get("followups", []):
        if "message" not in f:
            raise ValueError(f"{path}: followup without message")


def load(path) -> TaskSpec:
    d = json.loads(pathlib.Path(path).read_text(encoding="utf-8"))
    _validate(d, path)
    return TaskSpec(id=d["id"], set=d["set"], lang=d["lang"], site=d["site"],
                    task=d["task"], followups=d.get("followups", []),
                    checks=d["checks"], tags=d.get("tags", []),
                    mode=d.get("mode", "browser"), timeout_secs=d.get("timeout_secs", 300))


def load_dir(directory) -> list:
    specs = [load(p) for p in sorted(pathlib.Path(directory).glob("*.json"))]
    ids = [s.id for s in specs]
    dup = {i for i in ids if ids.count(i) > 1}
    if dup:
        raise ValueError(f"duplicate task ids: {sorted(dup)}")
    return specs


def render(spec: TaskSpec, base_url: str) -> TaskSpec:
    sub = lambda s: s.replace("{base}", base_url.rstrip("/"))
    return dataclasses.replace(
        spec, task=sub(spec.task),
        followups=[{**f, "message": sub(f["message"])} for f in spec.followups])
