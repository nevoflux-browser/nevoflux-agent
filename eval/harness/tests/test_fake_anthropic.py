import unittest

from eval.harness.fake_anthropic import assistant_turns, first_user_text, script


class FakeAnthropicTest(unittest.TestCase):
    def test_the_script_walks_tools_then_answers(self):
        user = "On http://localtest.me:5000/flights/index.html find the cheapest flight"
        self.assertEqual(script(0, user), ("tool", "browser_navigate", {"url": "http://localtest.me:5000/flights/index.html"}))
        self.assertEqual(script(1, user)[1], "browser_get_markdown")
        self.assertEqual(script(2, user), script(3, user))  # a repeat, to trigger a correction
        self.assertEqual(script(4, user)[0], "text")

    def test_turns_and_user_text_are_read_from_the_request(self):
        body = {"messages": [
            {"role": "user", "content": [{"type": "text", "text": "go to http://a.example/"}]},
            {"role": "assistant", "content": []},
            {"role": "user", "content": []},
        ]}
        self.assertEqual(assistant_turns(body), 1)
        self.assertEqual(first_user_text(body), "go to http://a.example/")


if __name__ == "__main__":
    unittest.main()
