import json
import unittest

from eval.harness.gate import gate, render


def trial(rep, tid, ok, inp=1000, out=10, jev_calls=0, jev_fallbacks=0, jev_in=0,
          status="succeeded", waits=()):
    usage = {"main": {"input": inp, "output": out}}
    if jev_calls or jev_fallbacks:
        usage["jev"] = {"input": jev_in, "output": 0, "calls": jev_calls, "fallbacks": jev_fallbacks}
    events = [{"type": "turn/start", "turn": 1}]
    for i, ms in enumerate(waits or [0]):
        events.append({"type": "step/start", "step": i + 1, "turn": 1})
        if ms:
            events.append({"type": "jev/wait", "site": "signals", "ms": ms})
    events.append({"type": "turn/end", "turn": 1})
    return {"rep": rep, "task_id": tid, "pass": ok, "status": status, "usage": [usage],
            "session_jsonl": "\n".join(json.dumps(e) for e in events)}


TASKS = ["jev-a", "jev-b", "jev-c", "jev-d", "jev-zh-e"]


def arm(pass_ids, k=3, jev=False, **kw):
    """Every task in TASKS, k reps; tasks in pass_ids pass."""
    j = {"jev_calls": 5, "jev_in": 100} if jev else {}
    return [trial(r, t, t in pass_ids, **j, **kw) for r in range(k) for t in TASKS]


class GateTest(unittest.TestCase):
    # n = 5 tasks, so δ = 0.2 whenever the off arm's per-rep score is constant;
    # C = input + 5·output + Jev input.

    def test_quality_path_passes_when_jev_wins_by_delta_within_cost(self):
        v = gate(arm(TASKS[:3]), arm(TASKS, jev=True, inp=1100))
        self.assertTrue(v["valid"], v["problems"])
        self.assertAlmostEqual(v["delta"], 0.2)
        self.assertAlmostEqual(v["score"]["diff"], 0.4)
        self.assertTrue(v["paths"]["quality"])
        self.assertIsNone(v["paths"]["efficiency"])
        self.assertTrue(v["pass"])

    def test_too_expensive_fails_the_quality_path(self):
        v = gate(arm(TASKS[:3]), arm(TASKS, jev=True, inp=2000))
        self.assertGreater(v["cost"]["change"], 0.25)
        self.assertFalse(v["paths"]["quality"])
        self.assertFalse(v["pass"])

    def test_a_tie_on_a_steady_baseline_is_not_a_quality_pass(self):
        v = gate(arm(TASKS), arm(TASKS, jev=True))
        self.assertEqual(v["score"]["diff"], 0)
        self.assertFalse(v["paths"]["quality"])
        self.assertFalse(v["pass"])

    def test_efficiency_path_needs_x(self):
        off, on = arm(TASKS), arm(TASKS, jev=True, inp=500)
        self.assertIsNone(gate(off, on)["paths"]["efficiency"])
        v = gate(off, on, x=20)
        self.assertTrue(v["paths"]["efficiency"])
        self.assertTrue(v["pass"])

    def test_missing_trials_make_the_gate_invalid(self):
        on = arm(TASKS, jev=True)
        on[0]["status"] = "provider_error"
        on[1]["status"] = "timeout"
        v = gate(arm(TASKS[:3]), on)
        self.assertFalse(v["valid"])
        self.assertFalse(v["pass"])
        self.assertTrue(any("missing" in p for p in v["problems"]), v["problems"])

    def test_arms_with_different_tasks_are_refused(self):
        on = [r for r in arm(TASKS, jev=True) if r["task_id"] != "jev-d"]
        v = gate(arm(TASKS[:3]), on)
        self.assertFalse(v["valid"])
        self.assertTrue(any("task" in p for p in v["problems"]), v["problems"])

    def test_an_inactive_jev_arm_is_invalid(self):
        self.assertFalse(gate(arm(TASKS[:3]), arm(TASKS))["valid"])  # no Jev calls at all
        on = [trial(r, t, True, jev_calls=1, jev_fallbacks=3) for r in range(3) for t in TASKS]
        v = gate(arm(TASKS[:3]), on)
        self.assertFalse(v["valid"])
        self.assertTrue(any("Jev inactive" in p for p in v["problems"]), v["problems"])

    def test_slow_steps_fail_the_latency_gate(self):
        v = gate(arm(TASKS[:3]), arm(TASKS, jev=True, waits=(400, 300, 0)))
        self.assertEqual(v["latency"]["p50"], 300)
        self.assertFalse(v["hard_gates"]["latency"])
        self.assertFalse(v["pass"])

    def test_a_chinese_regression_fails_even_when_quality_passes(self):
        off = arm(["jev-zh-e"])
        on = arm(["jev-a", "jev-b", "jev-c", "jev-d"], jev=True)
        v = gate(off, on)
        self.assertTrue(v["paths"]["quality"])
        self.assertTrue(v["zh"]["regressed"])
        self.assertFalse(v["pass"])

    def test_tools_comparison_says_split_when_tools_cost_more_without_gain(self):
        off = arm(TASKS[:3])
        on = arm(TASKS, jev=True, inp=1200)
        notools = arm(TASKS, jev=True, inp=1000)
        self.assertEqual(gate(off, on, notools)["tools"]["decision"], "split")
        # Tools beat no-tools by more than δ (1.0 vs 0.6): kept even though dearer.
        tools_help = arm(TASKS[:3], jev=True, inp=1000)
        self.assertEqual(gate(off, on, tools_help)["tools"]["decision"], "keep")

    def test_low_confidence_usage_is_a_warning_not_a_failure(self):
        on = arm(TASKS, jev=True)
        for r in on:
            r["usage"][0]["main"]["estimated"] = True
        v = gate(arm(TASKS[:3]), on)
        self.assertTrue(v["valid"])
        self.assertTrue(any("estimated" in w for w in v["warnings"]), v["warnings"])

    def test_report_states_the_verdict_and_the_numbers(self):
        md = render(gate(arm(TASKS[:3]), arm(TASKS, jev=True, inp=1100)))
        self.assertIn("G2: PASS", md)
        self.assertIn("quality", md)
        self.assertIn("P50", md)
        md = render(gate(arm(TASKS[:3]), arm(TASKS)))
        self.assertIn("G2: INVALID", md)


class GateThresholdTest(unittest.TestCase):
    def test_a_gain_of_exactly_delta_passes_the_quality_path(self):
        tasks = [f"jev-t{i:02d}" for i in range(30)]
        for n in range(1, 29):
            off = [trial(r, t, i < n) for r in range(3) for i, t in enumerate(tasks)]
            on = [trial(r, t, i < n + 1, jev_calls=1, jev_in=10)
                  for r in range(3) for i, t in enumerate(tasks)]
            v = gate(off, on)
            self.assertTrue(v["paths"]["quality"], (n, v["score"], v["delta"]))

    def test_one_chinese_trial_flip_at_delta_is_not_a_regression(self):
        # 20 + 10 zh tasks, k = 3, a steady baseline: δ is the 1/30 floor, and
        # one zh trial is 1/30 of the zh score — exactly δ, not a regression.
        tasks = [f"jev-t{i:02d}" for i in range(20)] + [f"jev-zh-t{i:02d}" for i in range(10)]
        zh = [t for t in tasks if "-zh-" in t]
        for m in range(1, len(zh) + 1):
            off = [trial(r, t, "-zh-" not in t or t in zh[:m]) for r in range(3) for t in tasks]
            on = [trial(r, t, "-zh-" not in t or t in zh[:m] and not (r == 0 and t == zh[0]),
                        jev_calls=1)
                  for r in range(3) for t in tasks]
            v = gate(off, on)
            self.assertAlmostEqual(v["delta"], 1 / 30)
            self.assertFalse(v["zh"]["regressed"], (m, v["zh"]))


class GateMissingTest(unittest.TestCase):
    def test_trials_missing_in_one_arm_are_left_out_of_every_arm(self):
        off = arm(TASKS[:3])
        on = arm(TASKS[:4], jev=True)
        lost = next(r for r in on if r["rep"] == 0 and r["task_id"] == "jev-a")
        lost["status"], lost["pass"] = "provider_error", False
        v = gate(off, on)
        self.assertTrue(v["valid"], v["problems"])
        # Without (rep 0, jev-a) in either arm: off 0.567, on 0.783.
        self.assertAlmostEqual(v["score"]["diff"], 0.2167, places=3)
        self.assertTrue(v["paths"]["quality"])
        self.assertTrue(any("left out" in w for w in v["warnings"]), v["warnings"])


class GateCliTest(unittest.TestCase):
    def test_cli_writes_the_report_and_exits_by_verdict(self):
        import pathlib
        import tempfile
        from eval.harness.gate import main
        with tempfile.TemporaryDirectory() as d:
            root = pathlib.Path(d)
            for name, rows in (("off", arm(TASKS[:3])), ("on", arm(TASKS, jev=True, inp=1100))):
                (root / name).mkdir()
                (root / name / "trials.jsonl").write_bytes(
                    "\n".join(json.dumps(r) for r in rows).encode("utf-8"))
            out = root / "g2" / "report.md"
            code = main(["--off", str(root / "off"), "--on", str(root / "on"), "--out", str(out)])
            self.assertEqual(code, 0)
            self.assertIn("G2: PASS", out.read_text(encoding="utf-8"))
            self.assertTrue(json.loads((root / "g2" / "report.md.json").read_text(encoding="utf-8"))["pass"])

    def test_cli_names_a_missing_results_directory(self):
        from eval.harness.gate import main
        with self.assertRaises(SystemExit) as e:
            main(["--off", "no-such-dir", "--on", "no-such-dir-either"])
        self.assertIn("no-such-dir", str(e.exception.code))


if __name__ == "__main__":
    unittest.main()
