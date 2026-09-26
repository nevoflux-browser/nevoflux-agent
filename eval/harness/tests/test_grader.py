import unittest

from eval.harness.grader import grade
from eval.harness.taskspec import TaskSpec


def spec(checks):
    return TaskSpec(id="x", set="j20", lang="en", site="shop", task="t",
                    followups=[], checks=checks, tags=[])


def ev(kind, trial="t1", **data):
    return {"trial": trial, "ts": 0.0, "site": "shop", "kind": kind, "data": data}


class GraderTest(unittest.TestCase):
    def test_output_regex_last_turn_by_default(self):
        r = grade(spec([{"type": "output_regex", "pattern": r"\$129"}]),
                  {"turn_outputs": ["$99", "$129"], "output": "$129", "events": [], "status": "succeeded"})
        self.assertTrue(r["pass"])

    def test_output_regex_specific_turn(self):
        r = grade(spec([{"type": "output_regex", "pattern": "alpha", "turn": 0}]),
                  {"turn_outputs": ["alpha", "beta"], "output": "beta", "events": [], "status": "succeeded"})
        self.assertTrue(r["pass"])

    def test_output_regex_matches_chinese(self):
        r = grade(spec([{"type": "output_regex", "pattern": "差评.{0,6}3"}]),
                  {"turn_outputs": [], "output": "一共有差评 3 条", "events": [], "status": "succeeded"})
        self.assertTrue(r["pass"])

    def test_output_regex_fails_without_output(self):
        r = grade(spec([{"type": "output_regex", "pattern": "."}]),
                  {"turn_outputs": [], "output": None, "events": [], "status": "failed"})
        self.assertFalse(r["pass"])

    def test_event_where_subset(self):
        r = grade(spec([{"type": "event", "kind": "order", "where": {"sku": "K-2"}}]),
                  {"turn_outputs": [], "output": "", "events": [ev("order", sku="K-2", qty=1)], "status": "failed"})
        self.assertTrue(r["pass"])

    def test_event_count_catches_double_submit(self):
        r = grade(spec([{"type": "event_count", "kind": "slow_done", "op": "==", "n": 1}]),
                  {"turn_outputs": [], "output": "", "events": [ev("slow_done"), ev("slow_done")], "status": "succeeded"})
        self.assertFalse(r["pass"])

    def test_no_event(self):
        r = grade(spec([{"type": "no_event", "kind": "decoy_click"}]),
                  {"turn_outputs": [], "output": "", "events": [ev("decoy_click")], "status": "succeeded"})
        self.assertFalse(r["pass"])

    def test_grader_ignores_other_trial_events(self):
        r = grade(spec([{"type": "no_event", "kind": "order"}]),
                  {"turn_outputs": [], "output": "", "events": [ev("order", trial="t0")],
                   "status": "succeeded", "trial": "t1"})
        self.assertTrue(r["pass"])

    def test_all_checks_must_pass(self):
        r = grade(spec([{"type": "output_regex", "pattern": "ok"}, {"type": "event", "kind": "order", "where": {}}]),
                  {"turn_outputs": [], "output": "ok", "events": [], "status": "succeeded"})
        self.assertFalse(r["pass"])
        self.assertEqual([c["ok"] for c in r["checks"]], [True, False])


if __name__ == "__main__":
    unittest.main()
