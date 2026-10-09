import importlib.util
import json
import pathlib
import re
import unittest

HERE = pathlib.Path(__file__).resolve().parents[1]


def mod(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


class Jev2CandidatesTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.make = mod("jev2_make", HERE / "tasks" / "jev2-candidates" / "_make.py")
        cls.cands = cls.make.make_candidates()

    def test_the_committed_tasks_match_a_fresh_generation(self):
        fresh = {t["id"]: t for t in self.cands}
        on_disk = {p.stem: json.loads(p.read_text(encoding="utf-8"))
                   for p in (HERE / "tasks" / "jev2-candidates").glob("*.json")}
        self.assertEqual(fresh, on_disk)

    def test_candidate_shape(self):
        c = self.cands
        self.assertGreaterEqual(len(c), 40)
        self.assertGreaterEqual(sum(t["lang"] == "zh" for t in c) * 3, len(c))
        self.assertGreaterEqual(sum(bool(t["followups"]) for t in c) * 3, len(c))
        self.assertGreaterEqual(sum("long" in t["tags"] for t in c), 5)
        self.assertTrue(all(t["site"] in ("ops", "ops-zh") for t in c))
        self.assertEqual(len({t["id"] for t in c}), len(c))

    def test_answer_patterns_reject_near_misses(self):
        # Numbers and versions: the kinds whose answer can be a prefix of a
        # wrong one. Patterns start with ANSWER[:：].
        for t in self.cands:
            if t["params"]["kind"] not in {"count", "change", "backref", "deploy"}:
                continue
            want = t["answer"]
            checks = [ch for ch in t["checks"] if ch["type"] == "output_regex" and ch["pattern"].startswith("ANSWER")]
            self.assertTrue(checks, t["id"])
            final = checks[-1]["pattern"]
            self.assertRegex(f"ANSWER: {want}", final)
            self.assertRegex(f"ANSWER：{want}", final)
            for bad in (f"ANSWER: {want}0", f"ANSWER: {want}.1", f"ANSWER: 9{want}"):
                self.assertNotRegex(bad, final, (t["id"], bad))

    def test_every_answer_is_the_truth(self):
        d = mod("ops_data", HERE / "sites" / "ops" / "data.py")
        portals = {"en": d.build("en"), "zh": d.build("zh")}
        for t in self.cands:
            self.assertEqual(t["answer"], self.make.truth_for(t, portals[t["lang"]], d), t["id"])

    def test_the_tasks_load_as_specs(self):
        from eval.harness.taskspec import load_dir
        specs = load_dir(HERE / "tasks" / "jev2-candidates")
        self.assertEqual(len(specs), len(self.cands))


class SelectTest(unittest.TestCase):
    def setUp(self):
        self.s = mod("select_tasks", HERE / "select_tasks.py")

    def meta(self, ids, **kw):
        return {i: {"lang": "zh" if i.startswith("z") else "en", "multi": "m" in i,
                    "long": "l" in i, "infra_fail": False, **kw} for i in ids}

    def test_selection_lands_in_the_band_and_keeps_the_shape(self):
        ids = [f"{l}{k:02d}{'m' if k % 2 else ''}{'l' if k % 3 == 0 else ''}" for l in "ez" for k in range(30)]
        rates = {i: (0.5 if n % 3 == 0 else 1.0) for n, i in enumerate(ids)}
        picked = self.s.select(rates, self.meta(ids), n=30)
        self.assertIsNotNone(picked)
        self.assertEqual(len(picked), 30)
        mean = sum(rates[i] for i in picked) / 30
        self.assertTrue(0.70 <= mean <= 0.85, mean)
        self.assertGreaterEqual(sum(i.startswith("z") for i in picked) * 3, 30)
        self.assertGreaterEqual(sum("m" in i for i in picked) * 3, 30)

    def test_all_easy_has_no_selection(self):
        ids = [f"e{k:02d}" for k in range(40)]
        self.assertIsNone(self.s.select({i: 1.0 for i in ids}, self.meta(ids), n=30))

    def test_infra_failures_are_never_picked(self):
        ids = [f"{l}{k:02d}m" for l in "ez" for k in range(20)]
        rates = {i: (0.5 if k < 12 else 1.0) for k, i in enumerate(ids)}
        m = self.meta(ids)
        m[ids[0]]["infra_fail"] = True
        rates[ids[0]] = 0.0
        r = self.s.select(rates, m, n=30)
        self.assertIsNotNone(r)
        self.assertNotIn(ids[0], r)

    def test_at_most_three_always_fail_tasks(self):
        ids = [f"{l}{k:02d}m" for l in "ez" for k in range(25)]
        rates = {i: (0.0 if k % 2 else 1.0) for k, i in enumerate(ids)}
        r = self.s.select(rates, self.meta(ids), n=30)
        if r is not None:
            self.assertLessEqual(sum(rates[i] == 0.0 for i in r), 3)
