import pathlib
import unittest

from eval.harness.taskspec import load_dir

TASKS = pathlib.Path(__file__).resolve().parents[1] / "tasks"
SITES = pathlib.Path(__file__).resolve().parents[1] / "sites"


class TaskSetTest(unittest.TestCase):
    def test_j20_shape(self):
        specs = load_dir(TASKS / "j20")
        self.assertGreaterEqual(len(specs), 20)
        self.assertTrue(all(s.set == "j20" for s in specs))

    def test_jev_shape(self):
        specs = load_dir(TASKS / "jev")
        self.assertGreaterEqual(len(specs), 30)
        zh = [s for s in specs if s.lang == "zh"]
        multi = [s for s in specs if s.followups]
        long_ = [s for s in specs if "long" in s.tags]
        self.assertGreaterEqual(len(zh) * 3, len(specs))
        self.assertGreaterEqual(len(multi) * 3, len(specs))
        self.assertGreaterEqual(len(long_), 3)

    def test_jev2_uncalibrated_shape(self):
        specs = load_dir(TASKS / "jev2-uncalibrated")
        self.assertGreaterEqual(len(specs), 30)
        self.assertTrue(all(s.set == "jev2-uncalibrated" for s in specs))
        self.assertGreaterEqual(sum(s.lang == "zh" for s in specs) * 3, len(specs))
        self.assertGreaterEqual(sum(bool(s.followups) for s in specs) * 3, len(specs))
        self.assertGreaterEqual(sum("long" in s.tags for s in specs), 5)

    def test_every_task_names_an_existing_site(self):
        from eval.harness.run import ensure_generated_sites
        ensure_generated_sites()
        for d in ("j20", "jev", "jev2-candidates", "jev2-uncalibrated"):
            if not (TASKS / d).is_dir():
                continue
            for s in load_dir(TASKS / d):
                self.assertTrue((SITES / s.site).is_dir(), f"{s.id}: {s.site}")


if __name__ == "__main__":
    unittest.main()
