#!/usr/bin/env python3
"""Deterministic OpenAI-compatible gateway for parity fixtures (port 14193).

First turn of a conversation: reasoning + one `read` tool call.
After the tool result: a markdown answer. No timestamps or random ids in output.
"""
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

REASONING = ["Let me look at ", "the file first."]
ANSWER = ["# Result\n\n", "The file says **hello**.\n\n", "- one\n- two\n\n", "```rust\nfn main() {}\n```\n"]


def chunk(delta, finish=None):
    return {"id": "c", "object": "chat.completion.chunk", "created": 0, "model": "fake-1",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def send_json(self, obj):
        b = json.dumps(obj).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def do_GET(self):
        if self.path.startswith("/v1/models"):
            self.send_json({"object": "list", "data": [{"id": "fake-1", "object": "model", "owned_by": "fake"}]})
        else:
            self.send_response(404); self.end_headers()

    def do_POST(self):
        n = int(self.headers.get("content-length", 0))
        req = json.loads(self.rfile.read(n) or b"{}")
        msgs = req.get("messages", [])
        after_tool = any(m.get("role") == "tool" for m in msgs)
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()
        out = []
        if after_tool:
            out += [chunk({"content": t}) for t in ANSWER]
            out.append(chunk({}, "stop"))
        else:
            out += [chunk({"reasoning_content": t}) for t in REASONING]
            out.append(chunk({"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
                       "function": {"name": "read", "arguments": json.dumps({"path": "/tmp/parity-cwd/hello.txt"})}}]}))
            out.append(chunk({}, "tool_calls"))
        out.append({"id": "c", "object": "chat.completion.chunk", "created": 0, "model": "fake-1", "choices": [],
                    "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}})
        for o in out:
            self.wfile.write(b"data: " + json.dumps(o).encode() + b"\n\n")
        self.wfile.write(b"data: [DONE]\n\n")


ThreadingHTTPServer(("127.0.0.1", 14193), H).serve_forever()
