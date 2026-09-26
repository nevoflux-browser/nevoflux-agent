import pathlib
import re
import unittest

SITES = pathlib.Path(__file__).resolve().parents[1] / "sites"
EXPECTED = {
    "widgets": ["overlay.html", "slow.html", "shadow.html", "select.html",
                "combobox.html", "scroll.html", "decoy.html", "iframe.html"],
    "shop": ["index.html", "search.html", "product.html", "cart.html", "checkout.html"],
    "flights": ["index.html", "results.html"],
    "messenger": ["index.html"],
    "zh-shop": ["index.html", "search.html", "product.html", "cart.html", "checkout.html"],
}


class SitesTest(unittest.TestCase):
    def test_expected_pages_exist(self):
        for site, pages in EXPECTED.items():
            for p in pages:
                self.assertTrue((SITES / site / p).exists(), f"{site}/{p}")

    def test_no_external_resources(self):
        for f in SITES.rglob("*.html"):
            text = f.read_text(encoding="utf-8")
            self.assertIsNone(re.search(r'(src|href)\s*=\s*"(https?:)?//', text), f)

    def test_interactive_pages_load_beacon(self):
        for site in EXPECTED:
            for p in EXPECTED[site]:
                text = (SITES / site / p).read_text(encoding="utf-8")
                self.assertIn("_lib/beacon.js", text, f"{site}/{p}")

    def test_no_inline_scripts_or_handlers(self):
        for f in SITES.rglob("*.html"):
            text = f.read_text(encoding="utf-8")
            self.assertIsNone(re.search(r"<script(?![^>]*\bsrc=)[^>]*>\s*\S", text), f"inline script in {f}")
            self.assertIsNone(re.search(r"\son[a-z]+\s*=", text), f"inline handler in {f}")

    def test_wiki_has_both_languages(self):
        self.assertTrue(list((SITES / "wiki" / "en").glob("*.html")))
        self.assertTrue(list((SITES / "wiki" / "zh").glob("*.html")))


if __name__ == "__main__":
    unittest.main()
