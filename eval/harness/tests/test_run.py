import unittest

from eval.harness.run import latest_rows, plan_trials, should_stop
from eval.harness.taskspec import TaskSpec


def spec(i):
    return TaskSpec(id=i, set="j20", lang="en", site="w", task="t", followups=[],
                    checks=[{"type": "no_event", "kind": "k"}], tags=[])


def row(rep, tid, status, ok=False):
    return {"rep": rep, "task_id": tid, "status": status, "pass": ok}


class ResumeTest(unittest.TestCase):
    def test_plan_skips_real_results_and_reruns_missing_ones(self):
        done = [row(0, "a", "succeeded", True), row(0, "b", "provider_error"),
                row(1, "a", "failed")]
        todo = [(rep, s.id) for rep, s in plan_trials([spec("a"), spec("b")], 2, done)]
        # (0,a) and (1,a) have real results; (0,b) was a provider error; (1,b) never ran.
        self.assertEqual(todo, [(0, "b"), (1, "b")])

    def test_latest_row_wins(self):
        rows = [row(0, "b", "provider_error"), row(0, "a", "succeeded", True),
                row(0, "b", "succeeded", True)]
        latest = latest_rows(rows)
        self.assertEqual(len(latest), 2)
        self.assertTrue(all(r["status"] == "succeeded" for r in latest))


class StopTest(unittest.TestCase):
    def test_stops_after_three_provider_errors_in_a_row(self):
        self.assertFalse(should_stop(["succeeded", "provider_error", "provider_error"]))
        self.assertTrue(should_stop(["succeeded", "provider_error", "provider_error", "provider_error"]))
        self.assertFalse(should_stop(["provider_error", "provider_error", "failed"]))


if __name__ == "__main__":
    unittest.main()


class SiteHostTest(unittest.TestCase):
    def test_site_host_must_resolve_to_loopback(self):
        from eval.harness.run import check_site_host
        check_site_host("127.0.0.1")
        check_site_host("localtest.me")  # public DNS → 127.0.0.1 (needs DNS)
        with self.assertRaises(SystemExit):
            check_site_host("example.com")
        with self.assertRaises(SystemExit):
            check_site_host("no-such-host.invalid")

    def test_trial_base_uses_the_site_host(self):
        from eval.harness.trial import site_base
        self.assertEqual(site_base("localtest.me", 5123, "shop"), "http://localtest.me:5123/shop")


class FreshAgentTest(unittest.TestCase):
    def test_a_binary_older_than_the_code_is_refused(self):
        import os
        import tempfile
        from eval.harness.run import check_agent_fresh
        with tempfile.TemporaryDirectory() as d:
            exe = os.path.join(d, "agent.exe")
            open(exe, "w").close()
            os.utime(exe, (1_000, 1_000))
            with self.assertRaises(SystemExit):
                check_agent_fresh(exe, d, commit_time=2_000)
            check_agent_fresh(exe, d, commit_time=500)
            with self.assertRaises(SystemExit):
                check_agent_fresh(os.path.join(d, "missing.exe"), d, commit_time=0)
