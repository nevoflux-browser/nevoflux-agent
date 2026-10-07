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


def ev(**kw):
    import json
    return json.dumps(kw)


class JevSummaryTest(unittest.TestCase):
    def test_jev_summary_reads_the_session_log(self):
        log = "\n".join([
            ev(type="turn/start", turn=1),
            ev(type="step/start", step=0, turn=1),
            ev(type="tool/call", id="a", name="read", args={}),
            ev(type="jev/signals", step=0, h=5, drift=0.1, irrelevant_bulk=None, needs_action=0.9, elapsed_ms=300),
            ev(type="step/start", step=1, turn=1),
            ev(type="jev/visibility", id="c1", tool="read", bytes=9000, level="short", graded_by="jev", kept_lines=0, elapsed_ms=400),
            ev(type="jev/fallback", point="visibility", reason="timeout", elapsed_ms=801),
            ev(type="step/start", step=2, turn=1),
            ev(type="jev/signals", step=2, h=1, drift=0.9, irrelevant_bulk=None, needs_action=0.2, elapsed_ms=280),
            ev(type="context/correction", step=3, triggers=["drift"], entries=1),
            ev(type="step/start", step=3, turn=1),
            ev(type="tool/call", id="b", name="recall", args={"chunk_id": "c1"}),
            ev(type="step/start", step=4, turn=1),
            ev(type="turn/end", turn=1),
        ])
        r = row(0, "a", True)
        r["session_jsonl"] = log
        j = summarize([r])["jev"]
        self.assertEqual(j["visibility"], {"short/jev": 1})
        self.assertEqual(j["fallbacks"], 1)
        self.assertEqual(j["signals"], 2)
        # last step 4 is the answer: tool steps left after 0 = 3, after 2 = 1
        self.assertAlmostEqual(j["h_mae"], 1.0)   # (|5-3| + |1-1|) / 2
        self.assertAlmostEqual(j["h_bias"], 1.0)  # (2 + 0) / 2
        self.assertEqual(j["corrections"], {"drift": 1})
        self.assertEqual(j["recalls"], 1)

    def test_jev_summary_counts_tool_sets(self):
        log = "\n".join([
            ev(type="turn/start", turn=1),
            ev(type="tools/select", reason="initial", names=["a", "b", "c", "d", "e"], added=[], removed=[], elapsed_ms=900),
            ev(type="tools/select", reason="missed", names=["a", "b", "c", "d", "e", "f", "g"], added=["f"], removed=[], elapsed_ms=0),
            ev(type="tools/select", reason="act", names=["a", "b", "c", "d", "e", "f", "g", "h", "i"], added=["h"], removed=[], elapsed_ms=0),
            ev(type="context/rebuild", reason="tool_change", decision="keep", keep_cost=1.0, rebuild_cost=2.0,
               h=3, before_tokens=10, after_tokens=10, regraded=0),
            ev(type="turn/end", turn=1),
        ])
        r = row(0, "a", True)
        r["session_jsonl"] = log
        j = summarize([r])["jev"]
        self.assertEqual(j["tool_sets"], {"initial": 1, "missed": 1, "act": 1})
        self.assertEqual(j["missed"], 2)
        self.assertEqual(j["tool_change_rebuilds"], 1)
        self.assertEqual(j["tool_set_size"], 5)   # turn-start sets only
        self.assertEqual(j["tool_select_ms"], 900)

    def test_jev_summary_counts_skill_injections(self):
        # The real order: the daemon logs skills/inject at turn start, before
        # the agent's turn/start; a subagent's nested turn sits inside.
        log = "\n".join([
            ev(type="skills/inject", name="research", p=0.9, elapsed_ms=600),
            ev(type="turn/start", turn=1),
            ev(type="tool/call", id="s", name="skill_load", args={"name": "research"}),
            ev(type="turn/end", turn=1),
            ev(type="skills/inject", name="cooking", p=0.85, elapsed_ms=600),
            ev(type="turn/start", turn=2),
            ev(type="tool/call", id="s", name="skill_load", args={"name": "cooking"}),
            ev(type="tool/call", id="sub", name="subagent_spawn", args={}),
            ev(type="turn/start", turn=1),
            ev(type="turn/end", turn=1),
            ev(type="tool/result", id="sub", content="done"),
            ev(type="tool/call", id="t", name="skill_load", args={"name": "brain"}),
            ev(type="turn/end", turn=2),
        ])
        r = row(0, "a", True)
        r["session_jsonl"] = log
        j = summarize([r])["jev"]
        self.assertEqual(j["skills"], {"injected": 2, "other_loaded": 1})

    def test_jev_summary_counts_permission_flags(self):
        log = "\n".join([
            ev(type="turn/start", turn=1),
            ev(type="permission/jev", tool="run_command", p=0.9, unattended=True),
            ev(type="permission/jev", tool="write", p=0.8, unattended=False),
            ev(type="turn/end", turn=1),
        ])
        r = row(0, "a", True)
        r["session_jsonl"] = log
        self.assertEqual(summarize([r])["jev"]["permissions"], {"flagged": 2, "unattended": 1})

    def test_effective_cost_weights_each_kind_of_token(self):
        from eval.harness.calibrate import DEFAULT_WEIGHTS, effective_cost
        r = {"usage": [
            {"main": {"input": 1000, "output": 10, "cache_read": 600, "cache_write": 200},
             "subagent": {"input": 100, "output": 0},
             "jev": {"input": 50, "output": 2, "calls": 1}},
        ]}
        # main: (1000-600-200)*1 + 200*1.25 + 600*0.1 + 10*5 = 200+250+60+50 = 560
        # subagent: 100; jev: 50*1 + 2*5 = 60
        self.assertAlmostEqual(effective_cost(r), 720.0)
        no_jev = {**DEFAULT_WEIGHTS, "jev_input": 0.0, "jev_output": 0.0}
        self.assertAlmostEqual(effective_cost(r, no_jev), 660.0)

    def test_step_waits_land_on_their_step_and_count_quiet_steps_as_zero(self):
        from eval.harness.calibrate import step_waits
        log = "\n".join([
            ev(type="jev/wait", site="turn_start", ms=700),   # before the turn: goes to its first step
            ev(type="turn/start", turn=1),
            ev(type="step/start", step=1, turn=1),
            ev(type="jev/wait", site="visibility", ms=300),
            ev(type="jev/wait", site="signals", ms=20),
            ev(type="step/start", step=2, turn=1),
            ev(type="step/start", step=3, turn=1),
            ev(type="turn/end", turn=1),
        ])
        self.assertEqual(step_waits({"session_jsonl": log}), [1020, 0, 0])

    def test_waits_with_no_following_step_still_count(self):
        from eval.harness.calibrate import step_waits
        log = "\n".join([
            ev(type="jev/wait", site="turn_start", ms=900),
            ev(type="turn/start", turn=1),
            ev(type="turn/end", turn=1),
        ])
        self.assertEqual(step_waits({"session_jsonl": log}), [900])

    def test_a_later_turns_start_wait_goes_to_that_turns_first_step(self):
        from eval.harness.calibrate import step_waits
        log = "\n".join([
            ev(type="jev/wait", site="turn_start", ms=700),
            ev(type="turn/start", turn=1),
            ev(type="step/start", step=1, turn=1),
            ev(type="jev/wait", site="signals", ms=20),
            ev(type="turn/end", turn=1),
            ev(type="jev/wait", site="turn_start", ms=900),   # logged before turn/start
            ev(type="turn/start", turn=2),
            ev(type="step/start", step=1, turn=2),
            ev(type="turn/end", turn=2),
        ])
        self.assertEqual(step_waits({"session_jsonl": log}), [720, 900])

    def test_a_turn_start_wait_with_no_step_is_its_own_step_before_the_next_turn(self):
        from eval.harness.calibrate import step_waits
        log = "\n".join([
            ev(type="jev/wait", site="turn_start", ms=900),
            ev(type="turn/start", turn=1),
            ev(type="turn/end", turn=1),
            ev(type="turn/start", turn=2),
            ev(type="step/start", step=1, turn=2),
            ev(type="turn/end", turn=2),
        ])
        self.assertEqual(step_waits({"session_jsonl": log}), [900, 0])

    def test_a_wait_after_a_nested_subagent_turn_goes_to_the_parents_step(self):
        from eval.harness.calibrate import step_waits
        log = "\n".join([
            ev(type="turn/start", turn=1),
            ev(type="step/start", step=1, turn=1),
            ev(type="turn/start", turn=1),                     # subagent, inside the parent's step
            ev(type="step/start", step=1, turn=1),
            ev(type="turn/end", turn=1),
            ev(type="jev/wait", site="signals", ms=50),        # the parent's step 1
            ev(type="step/start", step=2, turn=1),
            ev(type="turn/end", turn=1),
        ])
        self.assertEqual(step_waits({"session_jsonl": log}), [50, 0, 0])

    def test_summary_reports_cost_and_wait_percentiles(self):
        log = "\n".join([
            ev(type="turn/start", turn=1),
            ev(type="step/start", step=1, turn=1),
            ev(type="jev/wait", site="visibility", ms=400),
            ev(type="step/start", step=2, turn=1),
            ev(type="step/start", step=3, turn=1),
            ev(type="turn/end", turn=1),
        ])
        r = row(0, "a", True, inp=100)
        r["session_jsonl"] = log
        s = summarize([r])
        self.assertAlmostEqual(s["effective_cost"]["per_trial"], 100 + 10 * 5)
        self.assertEqual(s["jev_wait"]["steps"], 3)
        self.assertEqual(s["jev_wait"]["p50"], 0)
        self.assertEqual(s["jev_wait"]["p90"], 400)

    def test_no_jev_events_gives_empty_numbers(self):
        j = summarize([row(0, "a", True)])["jev"]
        self.assertEqual((j["signals"], j["h_mae"], j["h_bias"]), (0, None, None))
