import importlib.util
import pathlib
import unittest
from datetime import datetime

HERE = pathlib.Path(__file__).resolve().parents[1]


def ops():
    spec = importlib.util.spec_from_file_location("ops_data", HERE / "sites" / "ops" / "data.py")
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


class OpsDataTest(unittest.TestCase):
    def setUp(self):
        self.m = ops()

    def test_the_portal_is_deterministic_per_language(self):
        a, b = self.m.build("en"), self.m.build("en")
        self.assertEqual(a, b)
        self.assertNotEqual([i.cause for i in a.incidents], [i.cause for i in self.m.build("zh").incidents])

    def test_the_shape_makes_the_tasks_hard(self):
        p = self.m.build("en")
        self.assertEqual(len(p.incidents), 240)                       # 12 list pages
        self.assertEqual(len(p.services), 12)
        self.assertGreaterEqual(min(len("\n".join(i.log).encode()) for i in p.incidents), 8_000)
        self.assertTrue(all(i.prelim != i.cause for i in p.incidents))
        statuses = {s: sum(i.status == s for i in p.incidents) for s in ("open", "resolved", "archived")}
        self.assertTrue(all(n >= 20 for n in statuses.values()), statuses)
        titles = [i.title for i in p.incidents]
        self.assertLess(len(set(titles)), len(titles), "near-duplicate titles exist")

    def test_the_preliminary_cause_comes_before_the_confirmed_one(self):
        for i in self.m.build("en").incidents[:20]:
            text = "\n".join(i.log)
            self.assertLess(text.index(i.prelim), text.index(i.cause))

    def test_truths(self):
        m, p = self.m, self.m.build("en")
        svc = p.services[0]
        n = m.count_incidents(p, service=svc, severities={1, 2}, statuses={"open", "resolved"}, month=9)
        self.assertEqual(n, sum(1 for i in p.incidents if i.service == svc and i.severity in (1, 2)
                                and i.status != "archived" and i.opened.month == 9))
        inc = next(i for i in p.incidents if i.resolved)
        self.assertEqual(m.resolution_hours(p, inc.id), int((inc.resolved - inc.opened).total_seconds() // 3600))
        self.assertEqual(m.team_of_service(p, svc), p.team_of[svc])
        team = p.team_of[svc]
        who = m.oncall(p, team, inc.opened)
        self.assertIn((who, team), p.people)
        v = m.last_deploy_before(p, svc, datetime(2026, 9, 30, 23, 59))
        self.assertRegex(v, r"^v\d+\.\d+\.\d+$")

    def test_most_open_reports_ties_as_none(self):
        m, p = self.m, self.m.build("en")
        r = m.most_open(p, 1)
        if r is not None:
            svc, n = r
            others = [m.count_incidents(p, service=s, severities={1}, statuses={"open"}) for s in p.services if s != svc]
            self.assertTrue(all(o < n for o in others))

    def test_chinese_labels_and_names(self):
        p = self.m.build("zh")
        self.assertTrue(any("一" <= ch <= "鿿" for ch in p.services[0]))
        self.assertTrue(any("一" <= ch <= "鿿" for ch in p.people[0][0]))
        self.assertEqual(p.labels["severity"], "严重级别")
