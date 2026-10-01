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

    def test_jev_spend_is_summed_separately(self):
        r = row(0, "a", True, inp=1000)
        r["usage"][0]["jev"] = {"input": 300, "output": 20, "calls": 2}
        s = summarize([r, row(0, "b", True, inp=1000)])
        self.assertEqual(s["cost"]["input"], 2000, "Jev is not LLM input")
        self.assertEqual((s["cost"]["jev_input"], s["cost"]["jev_output"], s["cost"]["jev_calls"]), (300, 20, 2))

    def test_cache_totals_and_hit_ratio(self):
        r = row(0, "a", True, inp=1000)
        r["usage"][0]["main"]["cache_read"] = 800
        r["usage"][0]["main"]["cache_write"] = 100
        s = summarize([r, row(0, "b", True, inp=1000)])
        self.assertEqual((s["cost"]["cache_read"], s["cost"]["cache_write"]), (800, 100))
        self.assertAlmostEqual(s["cost"]["cache_hit_ratio"], 0.4)

    def test_cache_hit_ratio_is_zero_without_input(self):
        self.assertEqual(summarize([])["cost"]["cache_hit_ratio"], 0.0)

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
