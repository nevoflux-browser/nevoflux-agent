"""Synthetic ops portal for the jev2 eval set: data and the answers to the
questions tasks ask about it. Deterministic per language (seeded); the HTML
renderer (build.py) and the task generator (tasks/jev2/_make.py) both read
this module, so the site, its answers and the tasks cannot drift apart.

Everything here is invented: services, teams, people, incidents and logs.
"""

import random
from dataclasses import dataclass, field
from datetime import date, datetime, timedelta

SEED = {"en": 20261009, "zh": 20261010}
PER_PAGE = 20
CHANGES_PER_PAGE = 25
N_INCIDENTS = 240
LOG_LINES = (140, 320)
START = datetime(2026, 8, 1)
DAYS = 61                       # August and September 2026

TEXT = {
    "en": {
        "services": ["checkout", "search", "payments", "inventory", "auth", "notifications",
                     "shipping", "reviews", "catalog", "recommendations", "billing", "gateway"],
        "teams": ["Atlas", "Borealis", "Cinder", "Delta", "Ember", "Fable"],
        "first": ["Avery", "Blake", "Casey", "Devon", "Elliot", "Finley", "Gray", "Harper", "Indy", "Jordan"],
        "last": ["Marlow", "Quill", "Rowan"],
        "causes": ["expired TLS certificate on the internal load balancer",
                   "connection pool exhausted after a config rollback",
                   "disk full on the primary database replica",
                   "DNS record pointing to a decommissioned host",
                   "memory leak in the image resizing worker",
                   "rate limit misconfigured for the partner API",
                   "clock skew between cache nodes",
                   "schema migration locked the orders table",
                   "feature flag enabled for all regions at once",
                   "retry storm from the mobile client",
                   "stale CDN cache after a purge failure",
                   "message queue consumer stuck on a poison message",
                   "expired OAuth signing key",
                   "network partition between availability zones",
                   "runaway cron job saturating CPU"],
        "titles": ["Elevated error rate", "Latency spike", "Partial outage", "Failed deployment",
                   "Timeouts for some users", "Degraded performance", "Data inconsistency"],
        "levels": ["INFO", "INFO", "INFO", "WARN", "ERROR"],
        "msgs": ["health check passed", "request served", "retrying upstream call",
                 "cache miss for key", "worker heartbeat", "queue depth sampled",
                 "connection reused", "slow query logged", "user session refreshed"],
        "prelim": "PRELIMINARY CAUSE (unconfirmed): ",
        "cause": "ROOT CAUSE (confirmed): ",
        "labels": {"severity": "Severity", "status": "Status", "service": "Service", "opened": "Opened",
                   "resolved": "Resolved", "owner": "Owner", "title": "Title", "id": "ID",
                   "team": "Team", "name": "Name", "week": "Week starting", "oncall": "On call",
                   "version": "Version", "date": "Date", "note": "Note", "next": "Next page",
                   "prev": "Previous page", "incidents": "Incidents", "services": "Services",
                   "people": "People and on-call", "changes": "Changelog", "report": "File a report",
                   "count": "Count", "incident": "Incident", "submit": "Submit report",
                   "open": "open", "resolved_s": "resolved", "archived": "archived", "page": "Page",
                   "tier": "Tier", "log": "Log"},
    },
    "zh": {
        "services": ["结算", "搜索", "支付", "库存", "认证", "通知",
                     "物流", "评价", "商品目录", "推荐", "账单", "网关"],
        "teams": ["北辰组", "青岚组", "赤焰组", "沧澜组", "曜石组", "云栖组"],
        "first": ["子墨", "思远", "雨桐", "浩然", "若溪", "嘉懿", "明轩", "语嫣", "博文", "清欢"],
        "last": ["林", "苏", "沈"],
        "causes": ["内部负载均衡的 TLS 证书过期", "配置回滚后连接池耗尽", "主数据库副本磁盘写满",
                   "DNS 记录指向已下线的主机", "图片缩放进程内存泄漏", "合作方接口限流配置错误",
                   "缓存节点之间时钟不同步", "数据库迁移锁住了订单表", "功能开关被一次性对所有地区打开",
                   "移动客户端引发重试风暴", "CDN 清除失败导致缓存过期", "消息队列消费者卡在异常消息上",
                   "OAuth 签名密钥过期", "可用区之间网络分区", "失控的定时任务占满 CPU"],
        "titles": ["错误率升高", "延迟飙升", "部分服务中断", "发布失败", "部分用户请求超时",
                   "性能下降", "数据不一致"],
        "levels": ["INFO", "INFO", "INFO", "WARN", "ERROR"],
        "msgs": ["健康检查通过", "请求已处理", "重试上游调用", "缓存未命中", "工作进程心跳",
                 "采样队列深度", "复用连接", "记录慢查询", "刷新用户会话"],
        "prelim": "初步原因（未确认）：",
        "cause": "根本原因（已确认）：",
        "labels": {"severity": "严重级别", "status": "状态", "service": "服务", "opened": "开始时间",
                   "resolved": "解决时间", "owner": "负责人", "title": "标题", "id": "编号",
                   "team": "团队", "name": "姓名", "week": "值班周（周一）", "oncall": "值班人",
                   "version": "版本", "date": "日期", "note": "说明", "next": "下一页",
                   "prev": "上一页", "incidents": "故障列表", "services": "服务目录",
                   "people": "人员与值班", "changes": "变更记录", "report": "提交报告",
                   "count": "数量", "incident": "故障编号", "submit": "提交报告",
                   "open": "处理中", "resolved_s": "已解决", "archived": "已归档", "page": "第",
                   "tier": "等级", "log": "日志"},
    },
}


@dataclass(frozen=True)
class Incident:
    id: str
    title: str
    service: str
    severity: int
    status: str            # "open" | "resolved" | "archived"
    opened: datetime
    resolved: object       # datetime | None
    owner: str
    prelim: str
    cause: str
    log: tuple


@dataclass(frozen=True)
class Change:
    when: datetime
    service: str
    version: str
    note: str


@dataclass(frozen=True)
class Portal:
    lang: str
    services: tuple
    teams: tuple
    team_of: dict = field(hash=False)
    people: tuple = ()            # (name, team)
    oncall: tuple = ()            # (week start Monday, team, name)
    incidents: tuple = ()
    changes: tuple = ()           # newest first
    labels: dict = field(default_factory=dict, hash=False)


def _monday(d: date) -> date:
    return d - timedelta(days=d.weekday())


def _version_key(v: str):
    return tuple(int(x) for x in v[1:].split("."))


def build(lang: str) -> Portal:
    t = TEXT[lang]
    rnd = random.Random(SEED[lang])
    services, teams = tuple(t["services"]), tuple(t["teams"])
    team_of = {s: teams[i % len(teams)] for i, s in enumerate(services)}
    names = [(f"{f} {l}" if lang == "en" else f"{l}{f}") for l in t["last"] for f in t["first"]]
    people = tuple((n, teams[i % len(teams)]) for i, n in enumerate(names))   # 30 people, 5 per team
    members = {tm: [n for n, team in people if team == tm] for tm in teams}
    weeks = []
    w = _monday(START.date())
    while w <= (START + timedelta(days=DAYS)).date():
        weeks.append(w)
        w += timedelta(days=7)
    oncall = tuple((wk, tm, members[tm][(k + j) % len(members[tm])])
                   for k, wk in enumerate(weeks) for j, tm in enumerate(teams))

    incidents = []
    for n in range(1, N_INCIDENTS + 1):
        svc = rnd.choice(services)
        sev = rnd.choices([1, 2, 3, 4], weights=[10, 25, 40, 25])[0]
        opened = START + timedelta(minutes=rnd.randrange(DAYS * 24 * 60))
        status = rnd.choices(["open", "resolved", "archived"], weights=[25, 60, 15])[0]
        resolved = None if status == "open" else opened + timedelta(minutes=rnd.randrange(60, 96 * 60))
        owner = rnd.choice(members[team_of[svc]])
        cause, prelim = rnd.sample(t["causes"], 2)
        lines = rnd.randrange(*LOG_LINES)
        p_at, c_at = sorted(rnd.sample(range(10, lines - 5), 2))
        log, ts = [], opened
        for k in range(lines):
            ts = ts + timedelta(seconds=rnd.randrange(5, 240))
            stamp = ts.strftime("%Y-%m-%d %H:%M:%S")
            if k == p_at:
                log.append(f"{stamp} WARN {svc}: {t['prelim']}{prelim}")
            elif k == c_at:
                log.append(f"{stamp} ERROR {svc}: {t['cause']}{cause}")
            else:
                log.append(f"{stamp} {rnd.choice(t['levels'])} {svc}: {rnd.choice(t['msgs'])} "
                           f"#{rnd.randrange(10_000, 99_999)}")
        incidents.append(Incident(
            id=f"INC-{n:04d}", title=f"{rnd.choice(t['titles'])} — {svc}", service=svc,
            severity=sev, status=status, opened=opened, resolved=resolved, owner=owner,
            prelim=prelim, cause=cause, log=tuple(log)))

    raw = []
    for svc in services:
        major, minor, patch = rnd.randrange(1, 5), rnd.randrange(0, 9), 0
        for _ in range(rnd.randrange(8, 14)):
            patch += rnd.randrange(1, 4)
            if rnd.random() < 0.2:
                minor, patch = minor + 1, 0
            raw.append(Change(when=START + timedelta(minutes=rnd.randrange(DAYS * 24 * 60)),
                              service=svc, version=f"v{major}.{minor}.{patch}",
                              note=rnd.choice(t["msgs"])))
    # Versions rise with time per service, so "the last version before" is
    # well defined.
    changes = []
    for svc in services:
        cs = sorted((c for c in raw if c.service == svc), key=lambda c: c.when)
        versions = sorted((c.version for c in cs), key=_version_key)
        changes += [Change(c.when, c.service, v, c.note) for c, v in zip(cs, versions)]
    changes.sort(key=lambda c: c.when, reverse=True)

    return Portal(lang=lang, services=services, teams=teams, team_of=team_of, people=people,
                  oncall=oncall, incidents=tuple(incidents), changes=tuple(changes),
                  labels=dict(t["labels"]))


def count_incidents(p, service=None, severities=None, statuses=None, month=None) -> int:
    return sum(1 for i in p.incidents
               if (service is None or i.service == service)
               and (severities is None or i.severity in severities)
               and (statuses is None or i.status in statuses)
               and (month is None or i.opened.month == month))


def incident(p, inc_id):
    return next(i for i in p.incidents if i.id == inc_id)


def root_cause(p, inc_id) -> str:
    return incident(p, inc_id).cause


def team_of_service(p, service) -> str:
    return p.team_of[service]


def oncall(p, team, when) -> str:
    wk = _monday(when.date())
    return next(n for w, tm, n in p.oncall if w == wk and tm == team)


def last_deploy_before(p, service, when):
    before = [c for c in p.changes if c.service == service and c.when < when]
    return max(before, key=lambda c: c.when).version if before else None


def resolution_hours(p, inc_id) -> int:
    i = incident(p, inc_id)
    return int((i.resolved - i.opened).total_seconds() // 3600)


def most_open(p, severity):
    """(service, count) with the most open incidents of `severity`; None on a
    tie or when there are none."""
    counts = sorted(((count_incidents(p, service=s, severities={severity}, statuses={"open"}), s)
                     for s in p.services), reverse=True)
    if counts[0][0] == 0 or counts[0][0] == counts[1][0]:
        return None
    return counts[0][1], counts[0][0]
