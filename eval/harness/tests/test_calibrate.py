import unittest

from eval.harness.calibrate import summarize


def row(rep, tid, ok, status="succeeded", inp=100, est=False):
    return {"rep": rep, "task_id": tid, "pass": ok, "status": status,
            "usage": [{"main": {"input": inp, "output": 10, "calls": 1, "estimated": est}}]}


class CalibrateTest(unittest.TestCase):
    def test_delta_is_twice_sd_of_rep_scores(self):
        rows = [row(0, "a", True), row(0, "b", True),
                row(1, "a", True), row(1, "b", False),
                row(2, "a", False), row(2, "b", False)]
        s = summarize(rows)
        self.assertEqual(s["per_rep_score"], [1.0, 0.5, 0.0])
        self.assertAlmostEqual(s["sd"], 0.5)
        self.assertAlmostEqual(s["delta"], 1.0)
        self.assertEqual(sorted(s["flaky_tasks"]), ["a", "b"])

    def test_missing_trials_count_as_zero_and_can_invalidate(self):
        rows = [row(0, "a", False, status="harness_error"), row(0, "b", True)]
        s = summarize(rows)
        self.assertEqual(s["per_rep_score"], [0.5])
        self.assertTrue(s["eval_invalid"])  # 50% missing > 10%

    def test_cost_and_estimated_share(self):
        s = summarize([row(0, "a", True, inp=300, est=True), row(0, "b", True, inp=100)])
        self.assertEqual(s["cost"]["input"], 400)
        self.assertAlmostEqual(s["cost"]["estimated_share"], 0.5)

    def test_resumed_runs_count_only_the_latest_row(self):
        rows = [row(0, "a", False, status="provider_error"), row(0, "a", True)]
        s = summarize(rows)
        self.assertEqual(s["per_rep_score"], [1.0])
        self.assertEqual(s["missing_rate"], 0.0)

    def test_provider_errors_count_as_missing(self):
        rows = [row(0, "a", False, status="provider_error")] + [row(0, str(i), True) for i in range(4)]
        s = summarize(rows)
        self.assertAlmostEqual(s["missing_rate"], 0.2)
        self.assertTrue(s["eval_invalid"])


if __name__ == "__main__":
    unittest.main()
