"""Generate the jev2 candidate tasks from the ops portal's data.

python eval/harness/tasks/jev2/_make.py   # writes tasks/jev2-candidates/*.json

Every answer is computed by sites/ops/data.py; `truth_for` recomputes it
from a task's `params`, and a test checks the two agree, so editing the
portal cannot leave a stale answer behind.
"""

import importlib.util
import json
import pathlib
import random
import re

HERE = pathlib.Path(__file__).resolve().parent
HARNESS = HERE.parents[1]
OUT = HERE.parent / "jev2-candidates"
_spec = importlib.util.spec_from_file_location("ops_data", HARNESS / "sites" / "ops" / "data.py")
data = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(data)

SITE = {"en": "ops", "zh": "ops-zh"}
PER_KIND = 3
A = "ANSWER[:：]\\s*"            # the final line; zh replies may use a full-width colon


def num(n):
    return f"{A}{n}(?![\\d.])"


def ver(v):
    return f"{A}{re.escape(v)}(?![\\d.])"


def text(s):
    return re.escape(s)


def name(s):
    return f"{A}{re.escape(s)}(?!\\w)"


def check(pattern, turn=-1):
    return {"type": "output_regex", "pattern": pattern, "turn": turn}


END = {
    "en": "End your reply with a final line in the form 'ANSWER: {what}'.",
    "zh": "回复的最后一行写成“ANSWER: {what}”。",
}
WHAT = {
    "en": {"number": "<number>", "cause": "<the confirmed cause, copied exactly>", "name": "<name>",
           "version": "<version>", "hours": "<whole hours>"},
    "zh": {"number": "<数字>", "cause": "<已确认的原因，原文照抄>", "name": "<姓名>",
           "version": "<版本号>", "hours": "<整小时数>"},
}


def end(lang, what):
    return END[lang].format(what=WHAT[lang][what])


def _fmt(dt):
    return dt.strftime("%Y-%m-%d %H:%M")


# --- templates: each returns (prompt, followups, checks, answer, tags) or None


def t_count(lang, p, rnd):
    svc = rnd.choice(p.services)
    n = data.count_incidents(p, service=svc, severities={1, 2}, statuses={"open", "resolved"}, month=9)
    if n < 2:
        return None
    task = {
        "en": f"The incident list at {{base}}/incidents/page-1.html continues over several pages. How many "
              f"incidents for the {svc} service with severity SEV1 or SEV2 were opened in September 2026 and are "
              f"not archived? Check every page. {end(lang, 'number')}",
        "zh": f"{{base}}/incidents/page-1.html 的故障列表分好几页。“{svc}”服务在 2026 年 9 月开始、未归档、"
              f"严重级别为 SEV1 或 SEV2 的故障一共有多少条？请检查每一页。{end(lang, 'number')}",
    }[lang]
    return task, [], [check(num(n))], str(n), ["long"], {"service": svc}


def t_cause(lang, p, rnd):
    i = rnd.choice(p.incidents)
    task = {
        "en": f"Open {{base}}/incidents/{i.id}.html and find the confirmed root cause of this incident (the log "
              f"may also show an earlier, unconfirmed guess). {end(lang, 'cause')}",
        "zh": f"打开 {{base}}/incidents/{i.id}.html，找出这次故障已确认的根本原因（日志里可能还有更早的、"
              f"未确认的猜测）。{end(lang, 'cause')}",
    }[lang]
    return task, [], [check(A + text(i.cause))], i.cause, [], {"incident": i.id}


def t_join(lang, p, rnd):
    i = rnd.choice(p.incidents)
    team = data.team_of_service(p, i.service)
    who = data.oncall(p, team, i.opened)
    task = {
        "en": f"Open {{base}}/incidents/{i.id}.html. Which service did this incident affect, and when did it open?",
        "zh": f"打开 {{base}}/incidents/{i.id}.html。这次故障影响了哪个服务？是什么时候开始的？",
    }[lang]
    follow = {
        "en": [f"Which team owns that service? The service list is at {{base}}/services.html.",
               f"Who was on call for that team in the week the incident opened? The rota is on "
               f"{{base}}/people.html (weeks start on Monday). {end(lang, 'name')}"],
        "zh": [f"这个服务归哪个团队负责？服务目录在 {{base}}/services.html。",
               f"故障开始的那一周，这个团队的值班人是谁？值班表在 {{base}}/people.html（每周从周一算起）。"
               f"{end(lang, 'name')}"],
    }[lang]
    checks = [check(text(i.service), 0), check(text(team), 1), check(name(who), 2)]
    return task, [{"message": m} for m in follow], checks, who, ["multi"], {"incident": i.id}


def t_change(lang, p, rnd):
    s1, s2 = rnd.sample(list(p.services), 2)
    n1 = data.count_incidents(p, service=s1, severities={1}, statuses={"open"})
    n2 = data.count_incidents(p, service=s2, severities={1, 2}, statuses={"open"})
    if n1 < 1 or n2 < 2 or n1 == n2:
        return None
    task = {
        "en": f"Using all pages of the incident list starting at {{base}}/incidents/page-1.html, how many open "
              f"SEV1 incidents does the {s1} service have? {end(lang, 'number')}",
        "zh": f"根据从 {{base}}/incidents/page-1.html 开始的全部故障列表页，“{s1}”服务有多少条处理中的 SEV1 故障？"
              f"{end(lang, 'number')}",
    }[lang]
    follow = {
        "en": f"Sorry, I meant the {s2} service, and please count SEV1 and SEV2 together (still only open "
              f"incidents). {end(lang, 'number')}",
        "zh": f"抱歉，我说的是“{s2}”服务，而且请把 SEV1 和 SEV2 一起算（仍然只算处理中的）。{end(lang, 'number')}",
    }[lang]
    checks = [check(num(n1), 0), check(num(n2), 1)]
    return task, [{"message": follow}], checks, str(n2), ["multi", "long"], {"s1": s1, "s2": s2}


def t_backref(lang, p, rnd):
    done = [i for i in p.incidents if i.resolved is not None]
    x, y, z = rnd.sample(done, 3)
    h = data.resolution_hours(p, x.id)
    ask = {
        "en": "What is the confirmed root cause of the incident at {u}?",
        "zh": "{u} 这次故障已确认的根本原因是什么？",
    }[lang]
    task = ask.format(u=f"{{base}}/incidents/{x.id}.html")
    follow = {
        "en": [ask.format(u=f"{{base}}/incidents/{y.id}.html"),
               ask.format(u=f"{{base}}/incidents/{z.id}.html"),
               f"Going back to the first incident we looked at in this conversation: how many whole hours "
               f"passed between when it opened and when it was resolved? {end(lang, 'hours')}"],
        "zh": [ask.format(u=f"{{base}}/incidents/{y.id}.html"),
               ask.format(u=f"{{base}}/incidents/{z.id}.html"),
               f"回到这次对话里我们看的第一个故障：从开始到解决一共过了多少个整小时？{end(lang, 'hours')}"],
    }[lang]
    checks = [check(text(x.cause), 0), check(text(y.cause), 1), check(text(z.cause), 2), check(num(h), 3)]
    return (task, [{"message": m} for m in follow], checks, str(h), ["multi", "long"],
            {"incident": x.id, "others": [y.id, z.id]})


def t_deploy(lang, p, rnd):
    i = rnd.choice(p.incidents)
    v = data.last_deploy_before(p, i.service, i.opened)
    if v is None:
        return None
    task = {
        "en": f"Incident {i.id} ({{base}}/incidents/{i.id}.html) hit the {i.service} service. Using the changelog "
              f"at {{base}}/changes/page-1.html (newest first, several pages), what was the last version of "
              f"{i.service} deployed before the incident opened? {end(lang, 'version')}",
        "zh": f"故障 {i.id}（{{base}}/incidents/{i.id}.html）影响了“{i.service}”服务。根据 "
              f"{{base}}/changes/page-1.html 的变更记录（按时间倒序，共好几页），故障开始之前最后一次发布的"
              f"“{i.service}”版本是哪个？{end(lang, 'version')}",
    }[lang]
    return task, [], [check(ver(v))], v, ["long"], {"incident": i.id}


def t_report(lang, p, rnd, sev):
    r = data.most_open(p, sev)
    if r is None:
        return None
    svc, n = r
    task = {
        "en": f"Using every page of the incident list starting at {{base}}/incidents/page-1.html, find the service "
              f"with the most open SEV{sev} incidents. Then file a report at {{base}}/report.html with that service "
              f"and the number of its open SEV{sev} incidents (leave the incident field empty).",
        "zh": f"根据从 {{base}}/incidents/page-1.html 开始的全部故障列表页，找出处理中的 SEV{sev} 故障最多的服务，"
              f"然后在 {{base}}/report.html 提交一份报告，填写这个服务和它处理中的 SEV{sev} 故障数量（故障编号留空）。",
    }[lang]
    checks = [{"type": "event", "kind": "report", "where": {"service": svc, "count": str(n)}}]
    return task, [], checks, f"{svc}={n}", ["long", "write"], {"severity": sev}


KINDS = ["count", "cause", "join", "change", "backref", "deploy", "report"]
TIMEOUT = {"long": 900, "multi": 600}


def make_candidates() -> list:
    out = []
    for lang in ("en", "zh"):
        p = data.build(lang)
        for kind in KINDS:
            rnd = random.Random(f"jev2-{kind}-{lang}")
            made, tries, seen = 0, 0, set()
            while made < PER_KIND and tries < 200:
                tries += 1
                if kind == "report":
                    if tries > 4:
                        break
                    r = t_report(lang, p, rnd, sev=tries)
                else:
                    r = globals()[f"t_{kind}"](lang, p, rnd)
                if r is None:
                    continue
                task, followups, checks, answer, tags, params = r
                key = json.dumps(params, sort_keys=True)
                if key in seen:
                    continue
                seen.add(key)
                made += 1
                tags = ["jev2", kind] + tags
                timeout = 900 if "long" in tags else (600 if "multi" in tags else 300)
                out.append({
                    "id": f"jev2-{lang}-{kind}-{made}",
                    "set": "jev2-candidates",
                    "lang": lang,
                    "site": SITE[lang],
                    "task": task,
                    "followups": followups,
                    "checks": checks,
                    "tags": tags,
                    "timeout_secs": timeout,
                    "answer": answer,
                    "params": {"kind": kind, **params},
                })
    return out


def truth_for(task, p, d) -> str:
    """Recompute a task's answer from its params."""
    q = task["params"]
    kind = q["kind"]
    if kind == "count":
        return str(d.count_incidents(p, service=q["service"], severities={1, 2},
                                     statuses={"open", "resolved"}, month=9))
    if kind == "cause":
        return d.root_cause(p, q["incident"])
    if kind == "join":
        i = d.incident(p, q["incident"])
        return d.oncall(p, d.team_of_service(p, i.service), i.opened)
    if kind == "change":
        return str(d.count_incidents(p, service=q["s2"], severities={1, 2}, statuses={"open"}))
    if kind == "backref":
        return str(d.resolution_hours(p, q["incident"]))
    if kind == "deploy":
        i = d.incident(p, q["incident"])
        return d.last_deploy_before(p, i.service, i.opened)
    if kind == "report":
        svc, n = d.most_open(p, q["severity"])
        return f"{svc}={n}"
    raise ValueError(kind)


def main():
    OUT.mkdir(exist_ok=True)
    for old in OUT.glob("*.json"):
        old.unlink()
    for t in make_candidates():
        (OUT / f"{t['id']}.json").write_bytes(
            (json.dumps(t, ensure_ascii=False, indent=1) + "\n").encode("utf-8"))


if __name__ == "__main__":
    main()
