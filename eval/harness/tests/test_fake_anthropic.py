import unittest

from eval.harness.fake_anthropic import assistant_turns, first_user_text, position, script


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


class ActScriptTest(unittest.TestCase):
    def test_an_act_marker_scripts_act_then_the_loaded_tool(self):
        user = "go [fake:act] http://localtest.me:5000/flights/index.html"
        self.assertEqual(script(0, user)[1], "browser_navigate")
        self.assertEqual(script(1, user), ("tool", "act", {"intent": "think about the result"}))
        self.assertEqual(script(2, user), ("tool", "think", {"thought": "ok"}))
        self.assertEqual(script(3, user)[0], "text")


class FollowUpTest(unittest.TestCase):
    def test_a_follow_up_turn_reads_then_answers(self):
        self.assertEqual(script(0, "x", turn=1)[1], "browser_get_markdown")
        self.assertIn("ANSWER: NF752", script(1, "x", turn=1)[1])

    def test_position_counts_the_turn_and_its_steps(self):
        body = {"messages": [
            {"role": "user", "content": "first"},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "a", "name": "x", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "a", "content": "r"}]},
            {"role": "assistant", "content": "answer one"},
            {"role": "user", "content": "follow-up"},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "b", "name": "x", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "b", "content": "r"}]},
        ]}
        self.assertEqual(position(body), (1, 1))


if __name__ == "__main__":
    unittest.main()
