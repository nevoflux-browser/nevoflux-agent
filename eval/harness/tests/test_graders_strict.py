"""Wrong answers that a loose answer regex would accept must fail.

Each case is (task id, set, a wrong final answer, an accepted right answer).
"""

import pathlib
import unittest

from eval.harness.grader import grade
from eval.harness.taskspec import load

TASKS = pathlib.Path(__file__).resolve().parents[1] / "tasks"

CASES = [
    ("jev-shop-long-audit", "jev",
     "Iris Travel Mouse is rated 2.5 and Dune 2.8.\nANSWER: 4",
     "Five of them: K-4, M-3, M-6, H-3, P-4.\nANSWER: 5"),
    ("jev-zh-shop-long", "jev",
     "有一个评分 2.5 的商品。\n答案：4",
     "Z-1、Z-2、Z-4、Z-5、Z-7。\n答案：5"),
    ("jev-shop-compare-3", "jev",
     "Warranties: Aster 12, Borealis 24, Cobalt 36 months.\nANSWER: Borealis Low-Profile Keyboard",
     "Cobalt has 36 months.\nANSWER: Cobalt 60% Keyboard"),
    ("jev-flights-cheapest", "jev",
     "NF640 $199 (2 stops), NF752 $219 direct.\nANSWER: NF640",
     "ANSWER: NF752"),
    ("jev-zh-shop-cheapest", "jev",
     "矮轴机械键盘 超薄 ¥239、青轴 ¥299。\n答案：青轴机械键盘 87键 ¥299",
     "答案：矮轴机械键盘 超薄 ¥239"),
    ("j20-slow-report-effect", "j20",
     "I clicked Pay now once; the status is not paid yet.",
     "The final status text is: Paid"),
]


def result(text):
    return {"turn_outputs": [], "output": text, "events": [
        {"trial": "t", "kind": k, "data": {}} for k in ("pay_click", "pay_done", "search")],
        "status": "succeeded", "trial": "t"}


class StrictGraderTest(unittest.TestCase):
    def test_wrong_answers_fail_and_right_ones_pass(self):
        for tid, s, wrong, right in CASES:
            spec = load(TASKS / s / f"{tid}.json")
            self.assertFalse(grade(spec, result(wrong))["pass"], f"{tid} accepted: {wrong!r}")
            self.assertTrue(grade(spec, result(right))["pass"], f"{tid} rejected: {right!r}")

    def test_flights_followup_turn_two_must_name_only_the_direct_flight(self):
        spec = load(TASKS / "jev" / "jev-flights-followup.json")
        base = ["NF640 $199, NF752 $219, NF866 $226"]
        wrong = {"turn_outputs": base + ["NF640 has 2 stops, NF752 is direct, NF866 has 1 stop.\nANSWER: NF640"],
                 "output": "", "events": [], "status": "succeeded"}
        right = {"turn_outputs": base + ["ANSWER: NF752"], "output": "", "events": [], "status": "succeeded"}
        self.assertFalse(grade(spec, wrong)["pass"])
        self.assertTrue(grade(spec, right)["pass"])


if __name__ == "__main__":
    unittest.main()
