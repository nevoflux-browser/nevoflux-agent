import html
import importlib.util
import pathlib
import re
import unittest

HERE = pathlib.Path(__file__).resolve().parents[1]


def mod(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


class OpsSiteTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.b = mod("ops_build", HERE / "sites" / "ops" / "build.py")
        cls.d = cls.b.data
        cls.files = {lang: cls.b.render_all(lang) for lang in ("en", "zh")}

    def test_two_builds_are_byte_identical(self):
        self.assertEqual(self.b.render_all("en"), self.files["en"])
        self.assertEqual(self.b.render_all("zh"), self.files["zh"])

    def test_ensure_builds_missing_and_stale_sites_only(self):
        # The pages are not committed: ensure() writes them when they are
        # missing or built from older sources, and leaves a current build alone.
        import tempfile
        with tempfile.TemporaryDirectory() as d:
            roots = {"en": pathlib.Path(d) / "en", "zh": pathlib.Path(d) / "zh"}
            self.assertTrue(self.b.ensure(roots))
            page = roots["en"] / "incidents" / "page-1.html"
            self.assertEqual(page.read_bytes(), self.files["en"]["incidents/page-1.html"])
            self.assertEqual((roots["zh"] / "report.js").read_bytes(),
                             (HERE / "sites" / "ops" / "report.js").read_bytes())
            self.assertFalse(self.b.ensure(roots), "a current build is left alone")
            (roots["en"] / ".build-stamp").write_text("old", encoding="utf-8")
            self.assertTrue(self.b.ensure(roots), "a stale build is rebuilt")

    def test_incident_pages_are_large_and_lists_paginate(self):
        files = self.files["en"]
        self.assertIn("incidents/page-12.html", files)
        self.assertNotIn("incidents/page-13.html", files)
        sizes = [len(v) for k, v in files.items() if re.match(r"incidents/INC-\d+\.html", k)]
        self.assertEqual(len(sizes), 240)
        self.assertGreaterEqual(min(sizes), 8_000)
        self.assertIn(b'href="page-2.html"', files["incidents/page-1.html"])

    def test_counts_recomputed_from_the_html_equal_the_truth(self):
        for lang in ("en", "zh"):
            files, p = self.files[lang], self.d.build(lang)
            rows = []
            for k in sorted(files):
                if k.startswith("incidents/page-"):
                    rows += re.findall(r'<tr data-id="(INC-\d+)" data-service="([^"]*)" data-sev="(\d)" '
                                       r'data-status="(\w+)" data-opened="(\d{4}-\d{2})', files[k].decode())
            self.assertEqual(len(rows), 240)
            svc = p.services[1]
            got = sum(1 for _, s, sev, st, op in rows
                      if html.unescape(s) == svc and sev in "12" and st != "archived" and op == "2026-09")
            self.assertEqual(got, self.d.count_incidents(p, service=svc, severities={1, 2},
                                                         statuses={"open", "resolved"}, month=9))
            inc = p.incidents[7]
            page = files[f"incidents/{inc.id}.html"].decode()
            self.assertIn(html.escape(inc.cause), page)

    def test_the_chinese_site_is_utf8_and_chinese(self):
        idx = self.files["zh"]["incidents/page-1.html"].decode("utf-8")
        self.assertIn('<meta charset="utf-8">', idx)
        self.assertIn("严重级别", idx)
        self.assertIn('lang="zh-CN"', idx)

    def test_the_report_form_beacons_what_was_filed(self):
        js = (HERE / "sites" / "ops" / "report.js").read_text(encoding="utf-8")
        self.assertIn('nfEvent(f.dataset.site, "report"', js)
        self.assertIn("../_lib/beacon.js", self.files["en"]["report.html"].decode())
        self.assertIn('data-site="ops-zh"', self.files["zh"]["report.html"].decode())
