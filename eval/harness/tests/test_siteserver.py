import json
import pathlib
import tempfile
import time
import unittest
import urllib.request

from eval.harness.siteserver import SiteServer


def post(url, body=None):
    data = json.dumps(body or {}).encode()
    req = urllib.request.Request(url, data=data, method="POST",
                                 headers={"Content-Type": "application/json"})
    return urllib.request.urlopen(req, timeout=5).status


class SiteServerTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        root = pathlib.Path(self.dir.name)
        (root / "a.html").write_text("<title>中文标题</title>", encoding="utf-8")
        self.s = SiteServer(root)
        self.port = self.s.start()
        self.base = f"http://127.0.0.1:{self.port}"
        self.s.set_trial("t1")

    def tearDown(self):
        self.s.stop()
        self.dir.cleanup()

    def test_serves_static_utf8(self):
        body = urllib.request.urlopen(f"{self.base}/a.html", timeout=5).read().decode("utf-8")
        self.assertIn("中文标题", body)

    def test_records_events_for_current_trial(self):
        self.assertEqual(post(f"{self.base}/__event", {"site": "shop", "kind": "submit", "data": {"q": "键盘"}}), 204)
        ev = self.s.events()
        self.assertEqual(len(ev), 1)
        self.assertEqual((ev[0]["trial"], ev[0]["kind"], ev[0]["data"]["q"]), ("t1", "submit", "键盘"))

    def test_reset_clears_events(self):
        post(f"{self.base}/__event", {"site": "s", "kind": "k", "data": {}})
        post(f"{self.base}/__reset")
        self.assertEqual(self.s.events(), [])

    def test_events_are_scoped_to_trial(self):
        post(f"{self.base}/__event", {"site": "s", "kind": "k", "data": {}})
        self.s.set_trial("t2")
        self.assertEqual(self.s.events(), [])

    def test_slow_event_belongs_to_the_trial_that_sent_it(self):
        import threading
        t = threading.Thread(target=lambda: urllib.request.urlopen(
            f"{self.base}/__slow?ms=400&kind=pay_done&site=w", timeout=5).read())
        t.start()
        time.sleep(0.1)
        self.s.set_trial("t2")  # the next trial starts while the request is in flight
        t.join()
        self.assertEqual(self.s.events(), [], "late event leaked into the next trial")
        self.s.set_trial("t1")
        self.assertEqual([e["kind"] for e in self.s.events()], ["pay_done"])

    def test_slow_endpoint_delays_then_records(self):
        t0 = time.perf_counter()
        urllib.request.urlopen(f"{self.base}/__slow?ms=300&kind=slow_done&site=w", timeout=5).read()
        self.assertGreaterEqual(time.perf_counter() - t0, 0.29)
        self.assertEqual(self.s.events()[-1]["kind"], "slow_done")


if __name__ == "__main__":
    unittest.main()
