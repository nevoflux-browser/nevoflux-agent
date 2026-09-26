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


if __name__ == "__main__":
    unittest.main()
