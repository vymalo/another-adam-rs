#!/usr/bin/env python3
"""A stand-in OpenAI-compatible model server for the container smoke test.

Answers every POST .../chat/completions with one text-only completion
("Nothing to do."), streamed as SSE when the request asks for `stream`, plain
JSON otherwise, so the coder agent finishes its task without calling a tool.
Standard library only.

    fake-model.py [port]        (default 18080; listens on 127.0.0.1)
"""
import json
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

TEXT = "Nothing to do."
MODEL = "fake-model"


def completion():
    return {
        "id": "chatcmpl-fake",
        "object": "chat.completion",
        "created": int(time.time()),
        "model": MODEL,
        "choices": [
            {
                "index": 0,
                "message": {"role": "assistant", "content": TEXT},
                "finish_reason": "stop",
            }
        ],
        "usage": {"prompt_tokens": 1, "completion_tokens": 3, "total_tokens": 4},
    }


def chunk(delta, finish):
    return {
        "id": "chatcmpl-fake",
        "object": "chat.completion.chunk",
        "created": int(time.time()),
        "model": MODEL,
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
    }


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        sys.stderr.write("fake-model: " + fmt % args + "\n")

    def _reply(self, status, body, content_type="application/json"):
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path.rstrip("/").endswith("/models"):
            self._reply(200, {"object": "list", "data": [{"id": MODEL, "object": "model"}]})
        else:
            self._reply(200, {"status": "ok"})

    def do_POST(self):
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b"{}"
        try:
            request = json.loads(raw)
        except ValueError:
            self._reply(400, {"error": {"message": "invalid JSON"}})
            return
        if not self.path.rstrip("/").endswith("/chat/completions"):
            self._reply(404, {"error": {"message": "not found"}})
            return
        if request.get("stream"):
            frames = [
                chunk({"role": "assistant", "content": ""}, None),
                chunk({"content": TEXT}, None),
                chunk({}, "stop"),
            ]
            body = "".join("data: %s\n\n" % json.dumps(f) for f in frames) + "data: [DONE]\n\n"
            self._reply(200, body.encode(), "text/event-stream")
        else:
            self._reply(200, completion())


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 18080
    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    print("fake-model: listening on 127.0.0.1:%d" % port, file=sys.stderr, flush=True)
    server.serve_forever()
