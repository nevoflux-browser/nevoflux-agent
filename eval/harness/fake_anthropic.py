"""A scripted stand-in for an Anthropic Messages endpoint, for end-to-end
runs of the agent loop without a real model (no quota, deterministic).

Each request is answered from SCRIPT by how many assistant turns the request
already holds; streaming (`"stream": true`) and plain JSON are both served.

    python -m eval.harness.fake_anthropic PORT
    ... --set llm.anthropic.base_url=http://127.0.0.1:PORT
"""

import json
import re
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

URL = re.compile(r"https?://[^\s\"')]+")


def script(step: int, user_text: str, turn: int = 0):
    """(kind, payload) for the step-th assistant message of the turn-th turn."""
    if turn > 0:
        follow = [("tool", "browser_get_markdown", {})]
        if step < len(follow):
            return follow[step]
        return ("text", "The direct one is NF752.\nANSWER: NF752", None)
    m = URL.search(user_text)
    url = m.group(0) if m else "about:blank"
    if "[fake:act]" in user_text:
        # Jev tool assembly (P2-4): ask for a tool by intent, then use it.
        plan = [
            ("tool", "browser_navigate", {"url": url}),
            ("tool", "act", {"intent": "think about the result"}),
            ("tool", "think", {"thought": "ok"}),
        ]
        if step < len(plan):
            return plan[step]
        return ("text", "Done.\nANSWER: NF752", None)
    plan = [
        ("tool", "browser_navigate", {"url": url}),
        ("tool", "browser_get_markdown", {}),
        ("tool", "think", {"thought": "checking again"}),
        ("tool", "think", {"thought": "checking again"}),
    ]
    if step < len(plan):
        return plan[step]
    return ("text", "Done.\nANSWER: NF752", None)


def first_user_text(body) -> str:
    for m in body.get("messages", []):
        if m.get("role") != "user":
            continue
        c = m.get("content")
        if isinstance(c, str):
            return c
        for b in c or []:
            if isinstance(b, dict) and b.get("type") == "text":
                return b.get("text", "")
    return ""


def assistant_turns(body) -> int:
    return sum(1 for m in body.get("messages", []) if m.get("role") == "assistant")


def _is_user_text(m) -> bool:
    """A user message the person wrote (not a tool_result carrier)."""
    if m.get("role") != "user":
        return False
    c = m.get("content")
    if isinstance(c, str):
        return True
    return any(isinstance(b, dict) and b.get("type") == "text" for b in c or [])


def position(body):
    """(turn, step): which user turn this is, and how many assistant
    messages it already has."""
    msgs = body.get("messages", [])
    users = [i for i, m in enumerate(msgs) if _is_user_text(m)]
    if not users:
        return 0, assistant_turns(body)
    last = users[-1]
    step = sum(1 for m in msgs[last + 1:] if m.get("role") == "assistant")
    return len(users) - 1, step


def sse(event, data):
    return f"event: {event}\ndata: {json.dumps(data)}\n\n".encode()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        n = int(self.headers.get("content-length") or 0)
        body = json.loads(self.rfile.read(n) or b"{}")
        turn, step = position(body)
        kind, a, b = script(step, first_user_text(body), turn)
        usage = {"input_tokens": 100, "output_tokens": 10}
        if kind == "tool":
            block = {"type": "tool_use", "id": f"toolu_{step}", "name": a, "input": b}
            stop = "tool_use"
        else:
            block = {"type": "text", "text": a}
            stop = "end_turn"
        if body.get("stream"):
            out = sse("message_start", {"type": "message_start", "message": {
                "id": f"msg_{step}", "type": "message", "role": "assistant", "model": "fake",
                "content": [], "stop_reason": None, "usage": usage}})
            if kind == "tool":
                out += sse("content_block_start", {"type": "content_block_start", "index": 0,
                                                   "content_block": {**block, "input": {}}})
                out += sse("content_block_delta", {"type": "content_block_delta", "index": 0,
                                                   "delta": {"type": "input_json_delta",
                                                             "partial_json": json.dumps(b)}})
            else:
                out += sse("content_block_start", {"type": "content_block_start", "index": 0,
                                                   "content_block": {"type": "text", "text": ""}})
                out += sse("content_block_delta", {"type": "content_block_delta", "index": 0,
                                                   "delta": {"type": "text_delta", "text": a}})
            out += sse("content_block_stop", {"type": "content_block_stop", "index": 0})
            out += sse("message_delta", {"type": "message_delta", "delta": {"stop_reason": stop},
                                         "usage": {"output_tokens": 10}})
            out += sse("message_stop", {"type": "message_stop"})
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
        else:
            out = json.dumps({"id": f"msg_{step}", "type": "message", "role": "assistant",
                              "model": "fake", "content": [block], "stop_reason": stop,
                              "usage": usage}).encode()
            self.send_response(200)
            self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)

    def log_message(self, *a):
        pass


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
