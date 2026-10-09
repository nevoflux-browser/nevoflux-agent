"""Render the synthetic ops portal (data.py) into static pages.

python eval/harness/sites/ops/build.py   # writes sites/ops (en) and sites/ops-zh (zh)
"""

import html
import importlib.util
import pathlib

HERE = pathlib.Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location("ops_data", HERE / "data.py")
data = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(data)

ROOTS = {"en": HERE, "zh": HERE.parent / "ops-zh"}
SITE = {"en": "ops", "zh": "ops-zh"}
e = html.escape


def _page(lang, title, body, depth=0):
    up = "../" * depth
    L = data.TEXT[lang]["labels"]
    nav = (f'<nav><a href="{up}index.html">{e(L["incidents"])}</a> · '
           f'<a href="{up}services.html">{e(L["services"])}</a> · '
           f'<a href="{up}people.html">{e(L["people"])}</a> · '
           f'<a href="{up}changes/page-1.html">{e(L["changes"])}</a> · '
           f'<a href="{up}report.html">{e(L["report"])}</a></nav>')
    return (f'<!doctype html>\n<html lang="{"zh-CN" if lang == "zh" else "en"}">\n<meta charset="utf-8">\n'
            f"<title>{e(title)}</title>\n{nav}\n<h1>{e(title)}</h1>\n{body}\n</html>\n").encode("utf-8")


def _status(lang, s):
    L = data.TEXT[lang]["labels"]
    return {"open": L["open"], "resolved": L["resolved_s"], "archived": L["archived"]}[s]


def _fmt(dt):
    return dt.strftime("%Y-%m-%d %H:%M") if dt else "—"


def _links(L, n, pages):
    links = []
    if n > 1:
        links.append(f'<a href="page-{n - 1}.html">{e(L["prev"])}</a>')
    if n < pages:
        links.append(f'<a href="page-{n + 1}.html">{e(L["next"])}</a>')
    return " · ".join(links)


def _page_title(lang, L, what, n, pages):
    return f"{L[what]} — {L['page']} {n}/{pages}" if lang == "en" else f"{L[what]} — 第 {n}/{pages} 页"


def render_all(lang) -> dict:
    p = data.build(lang)
    L = p.labels
    out = {}

    pages = (len(p.incidents) + data.PER_PAGE - 1) // data.PER_PAGE
    head = "".join(f"<th>{e(L[k])}</th>" for k in ("id", "title", "service", "severity", "status", "opened", "owner"))
    for n in range(1, pages + 1):
        chunk = p.incidents[(n - 1) * data.PER_PAGE:n * data.PER_PAGE]
        rows = "".join(
            f'<tr data-id="{i.id}" data-service="{e(i.service)}" data-sev="{i.severity}" '
            f'data-status="{i.status}" data-opened="{i.opened.strftime("%Y-%m-%d")}">'
            f'<td><a href="{i.id}.html">{i.id}</a></td><td>{e(i.title)}</td><td>{e(i.service)}</td>'
            f"<td>SEV{i.severity}</td><td>{e(_status(lang, i.status))}</td><td>{_fmt(i.opened)}</td>"
            f"<td>{e(i.owner)}</td></tr>\n" for i in chunk)
        out[f"incidents/page-{n}.html"] = _page(
            lang, _page_title(lang, L, "incidents", n, pages),
            f"<table><tr>{head}</tr>\n{rows}</table>\n<p>{_links(L, n, pages)}</p>", depth=1)

    for i in p.incidents:
        meta = "".join(f"<dt>{e(L[k])}</dt><dd>{e(v)}</dd>" for k, v in (
            ("service", i.service), ("severity", f"SEV{i.severity}"), ("status", _status(lang, i.status)),
            ("opened", _fmt(i.opened)), ("resolved", _fmt(i.resolved)), ("owner", i.owner)))
        out[f"incidents/{i.id}.html"] = _page(
            lang, f"{i.id} — {i.title}",
            f"<dl>{meta}</dl>\n<h2>{e(L['log'])}</h2>\n<pre>{e(chr(10).join(i.log))}</pre>", depth=1)

    out["services.html"] = _page(lang, L["services"], "<table><tr>" + "".join(
        f"<th>{e(L[k])}</th>" for k in ("service", "team", "tier")) + "</tr>\n" + "".join(
        f"<tr><td>{e(s)}</td><td>{e(p.team_of[s])}</td><td>{1 + k % 3}</td></tr>\n"
        for k, s in enumerate(p.services)) + "</table>")

    people = "".join(f"<tr><td>{e(n)}</td><td>{e(t)}</td></tr>\n" for n, t in p.people)
    oncall = "".join(f"<tr><td>{w.isoformat()}</td><td>{e(t)}</td><td>{e(n)}</td></tr>\n"
                     for w, t, n in p.oncall)
    out["people.html"] = _page(
        lang, L["people"],
        f"<table><tr><th>{e(L['name'])}</th><th>{e(L['team'])}</th></tr>\n{people}</table>\n"
        f"<h2>{e(L['oncall'])}</h2>\n<table><tr><th>{e(L['week'])}</th><th>{e(L['team'])}</th>"
        f"<th>{e(L['oncall'])}</th></tr>\n{oncall}</table>")

    cp = (len(p.changes) + data.CHANGES_PER_PAGE - 1) // data.CHANGES_PER_PAGE
    chead = "".join(f"<th>{e(L[k])}</th>" for k in ("date", "service", "version", "note"))
    for n in range(1, cp + 1):
        chunk = p.changes[(n - 1) * data.CHANGES_PER_PAGE:n * data.CHANGES_PER_PAGE]
        rows = "".join(f"<tr><td>{_fmt(c.when)}</td><td>{e(c.service)}</td><td>{c.version}</td>"
                       f"<td>{e(c.note)}</td></tr>\n" for c in chunk)
        out[f"changes/page-{n}.html"] = _page(
            lang, _page_title(lang, L, "changes", n, cp),
            f"<table><tr>{chead}</tr>\n{rows}</table>\n<p>{_links(L, n, cp)}</p>", depth=1)

    done = "Report filed." if lang == "en" else "报告已提交。"
    out["report.html"] = _page(
        lang, L["report"],
        f'<script src="../_lib/beacon.js"></script>\n'
        f'<form id="report" data-site="{SITE[lang]}"><label>{e(L["service"])} <input name="service"></label>\n'
        f'<label>{e(L["count"])} <input name="count"></label>\n'
        f'<label>{e(L["incident"])} <input name="incident"></label>\n'
        f'<button type="submit">{e(L["submit"])}</button></form>\n'
        f'<p id="done" data-msg="{e(done)}"></p>\n<script src="report.js"></script>')
    if lang == "zh":
        out["report.js"] = (HERE / "report.js").read_bytes()
    out["index.html"] = _page(lang, L["incidents"],
                              f'<p><a href="incidents/page-1.html">{e(L["incidents"])}</a></p>')
    return out


def write(lang, root):
    for rel, blob in render_all(lang).items():
        path = pathlib.Path(root) / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(blob)


def _sources_hash() -> str:
    import hashlib
    h = hashlib.sha256()
    for name in ("data.py", "build.py", "report.js"):
        h.update((HERE / name).read_bytes())
    return h.hexdigest()


def ensure(roots=None) -> bool:
    """Build the sites if they are missing or were built from other sources.
    The pages are generated, not committed; the harness calls this before it
    serves them. Returns whether anything was (re)built."""
    roots = roots or ROOTS
    want = _sources_hash()
    built = False
    for lang, root in roots.items():
        stamp = pathlib.Path(root) / ".build-stamp"
        if stamp.exists() and stamp.read_text(encoding="utf-8") == want:
            continue
        write(lang, root)
        stamp.write_text(want, encoding="utf-8")
        built = True
    return built


def main():
    for lang, root in ROOTS.items():
        write(lang, root)


if __name__ == "__main__":
    main()
