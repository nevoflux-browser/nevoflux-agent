import tomllib
import unittest

from eval.harness.tomledit import override

BASE = '''\
[llm]
provider = "custom:chinamobile"  # my default

[llm.anthropic]
model = "opus"
'''


class TomlEditTest(unittest.TestCase):
    def test_replaces_existing_key_keeping_other_sections(self):
        out = override(BASE, "llm.provider", "anthropic")
        d = tomllib.loads(out)
        self.assertEqual(d["llm"]["provider"], "anthropic")
        self.assertEqual(d["llm"]["anthropic"]["model"], "opus")

    def test_nested_section_key(self):
        out = override(BASE, "llm.anthropic.model", "sonnet")
        self.assertEqual(tomllib.loads(out)["llm"]["anthropic"]["model"], "sonnet")

    def test_appends_missing_section_and_bool(self):
        out = override(BASE, "jev.enabled", True)
        self.assertIs(tomllib.loads(out)["jev"]["enabled"], True)

    def test_appends_missing_key_in_existing_section(self):
        out = override(BASE, "llm.default_model", "x")
        self.assertEqual(tomllib.loads(out)["llm"]["default_model"], "x")

    def test_does_not_touch_same_key_in_other_section(self):
        text = '[a]\nk = 1\n[b]\nk = 2\n'
        d = tomllib.loads(override(text, "b.k", 3))
        self.assertEqual((d["a"]["k"], d["b"]["k"]), (1, 3))

    def test_override_verifies_with_tomllib(self):
        # A value that TOML cannot represent as written must not pass silently.
        with self.assertRaises(ValueError):
            override(BASE, "llm.provider", object())


if __name__ == "__main__":
    unittest.main()
