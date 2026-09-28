"""Override one key in a TOML config's text, then verify with tomllib.

Line-based on purpose: the user's config has comments and ordering worth
keeping, and the harness only ever sets scalar keys.
"""

import json
import re
import tomllib

_SECTION = re.compile(r"^\s*\[([^\[\]]+)\]\s*(#.*)?$")


def _literal(value) -> str:
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, (int, float)):
        return repr(value)
    if isinstance(value, str):
        return json.dumps(value, ensure_ascii=False)
    raise ValueError(f"unsupported TOML value: {value!r}")


def _get(d, dotted):
    for part in dotted.split("."):
        d = d[part]
    return d


def override(text: str, dotted_key: str, value) -> str:
    section, _, key = dotted_key.rpartition(".")
    if not section or not key:
        raise ValueError(f"need section.key, got {dotted_key!r}")
    line = f"{key} = {_literal(value)}"
    key_re = re.compile(rf"^\s*{re.escape(key)}\s*=")

    lines = text.splitlines()
    current, start, end = None, None, len(lines)
    for i, l in enumerate(lines):
        m = _SECTION.match(l)
        if m:
            if current == section:
                end = i
                break
            current = m.group(1).strip()
            if current == section:
                start = i
    if start is None:
        lines += ["", f"[{section}]", line]
    else:
        for i in range(start + 1, end):
            if key_re.match(lines[i]):
                lines[i] = line
                break
        else:
            insert_at = end
            while insert_at > start + 1 and not lines[insert_at - 1].strip():
                insert_at -= 1
            lines.insert(insert_at, line)
    out = "\n".join(lines) + "\n"
    try:
        got = _get(tomllib.loads(out), dotted_key)
    except (tomllib.TOMLDecodeError, KeyError) as e:
        raise ValueError(f"override of {dotted_key} produced invalid TOML: {e}") from e
    if got != value:
        raise ValueError(f"override of {dotted_key} did not take: got {got!r}")
    return out
