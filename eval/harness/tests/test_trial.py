import socket
import unittest

from eval.harness import daemon
from eval.harness.trial import build_task_body
from eval.harness.taskspec import TaskSpec


class PollTest(unittest.TestCase):
    def test_returns_on_terminal(self):
        states = iter([{"status": "running"}, {"status": "succeeded", "output": "x"}])
        last, timed_out = daemon.poll(lambda: next(states), timeout_secs=100,
                                      interval=0, sleep=lambda s: None)
        self.assertEqual((last["status"], timed_out), ("succeeded", False))

    def test_poll_times_out(self):
        t = [0.0]

        def clock():
            t[0] += 10
            return t[0]

        last, timed_out = daemon.poll(lambda: {"status": "running"}, timeout_secs=25,
                                      interval=0, clock=clock, sleep=lambda s: None)
        self.assertTrue(timed_out)
        self.assertEqual(last["status"], "running")


class PortTest(unittest.TestCase):
    def test_free_port_is_bindable(self):
        p = daemon.free_port()
        s = socket.socket()
        s.bind(("127.0.0.1", p))
        s.close()


class BodyTest(unittest.TestCase):
    def test_body_carries_followups_and_no_retry(self):
        spec = TaskSpec(id="a", set="jev", lang="zh", site="shop", task="买键盘",
                        followups=[{"message": "再说一遍价格", "delay_secs": 360}],
                        checks=[{"type": "no_event", "kind": "x"}], tags=[], timeout_secs=900)
        body = build_task_body(spec)
        self.assertEqual(body["task"], "买键盘")
        self.assertEqual(body["followups"][0]["delay_secs"], 360)
        self.assertTrue(body["no_retry"])
        self.assertEqual(body["mode"], "browser")
        self.assertGreaterEqual(body["wall_clock_secs"], 900)


class RenderTest(unittest.TestCase):
    def test_render_root_placeholder(self):
        from eval.harness.taskspec import render
        s = TaskSpec(id="a", set="jev", lang="en", site="shop", task="{root}/wiki/en/x.html then {base}/index.html",
                     followups=[{"message": "{base}/cart.html"}], checks=[{"type": "no_event", "kind": "k"}], tags=[])
        r = render(s, "http://127.0.0.1:9/shop")
        self.assertEqual(r.task, "http://127.0.0.1:9/wiki/en/x.html then http://127.0.0.1:9/shop/index.html")
        self.assertEqual(r.followups[0]["message"], "http://127.0.0.1:9/shop/cart.html")


class StatusTest(unittest.TestCase):
    def test_provider_error_output_is_not_an_agent_result(self):
        from eval.harness.trial import classify_status
        last = {"status": "succeeded",
                "output": " [Error: ProviderError: Invalid status code 500 Internal Server Error ...]"}
        self.assertEqual(classify_status(last, timed_out=False), "provider_error")

    def test_provider_error_in_any_turn(self):
        from eval.harness.trial import classify_status
        last = {"status": "succeeded", "output": "fine",
                "turn_outputs": ["[Error: ProviderError: 429 ...]", "fine"]}
        self.assertEqual(classify_status(last, timed_out=False), "provider_error")

    def test_raw_path_provider_error_is_not_an_agent_result(self):
        # Since P0 the Anthropic wire goes through the daemon's raw path, whose
        # errors read "Internal error: Anthropic-raw …" instead of rig's
        # "ProviderError" — a quota 403 was counted as an agent failure.
        from eval.harness.trial import classify_status
        last = {"status": "succeeded",
                "output": '[Error: Internal error: Anthropic-raw stream HTTP 403 Forbidden: '
                          '{"error":{"type":"permission_error","message":"You\'ve reached your '
                          '5-hour usage limit."}}]'}
        self.assertEqual(classify_status(last, timed_out=False), "provider_error")

    def test_timeout_and_normal_status(self):
        from eval.harness.trial import classify_status
        self.assertEqual(classify_status({"status": "running"}, timed_out=True), "timeout")
        self.assertEqual(classify_status({"status": "failed", "output": "no"}, timed_out=False), "failed")


class EffectiveConfigTest(unittest.TestCase):
    LOG = (
        '2026 INFO nevoflux_daemon::server: Loaded agent config: llm.provider=Some("anthropic")\n'
        "2026 INFO nevoflux_daemon::server: LLM config set on services: provider=Anthropic, model=k3\n"
    )

    def test_parses_provider_and_model(self):
        eff = daemon.parse_effective_config(self.LOG)
        self.assertEqual((eff["provider"], eff["model"], eff["load_error"]), ("anthropic", "k3", None))

    def test_detects_load_failure(self):
        eff = daemon.parse_effective_config(
            "ERROR nevoflux_daemon::server: Failed to load agent config: bad type, using defaults\n")
        self.assertIn("bad type", eff["load_error"])

    def test_override_mismatch_is_an_error(self):
        from eval.harness.trial import check_effective
        eff = daemon.parse_effective_config(self.LOG)
        self.assertIsNone(check_effective({"llm.provider": "anthropic"}, eff))
        self.assertIn("custom:x", check_effective({"llm.provider": "custom:x"}, eff))
        self.assertIn("bad", check_effective({}, {"provider": None, "model": None, "load_error": "bad"}))

    def test_set_parses_floats_and_bools(self):
        from eval.harness.run import _parse_set
        self.assertEqual(_parse_set(["a.t=0.2", "a.n=3", "a.b=true", "a.s=x"]),
                         {"a.t": 0.2, "a.n": 3, "a.b": True, "a.s": "x"})


if __name__ == "__main__":
    unittest.main()
