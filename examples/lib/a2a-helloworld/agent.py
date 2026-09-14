#!/usr/bin/env python3
# Copyright 2026 The Drasi Authors.
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

"""Stdlib A2A JSON-RPC Hello World agent. No pip, no a2a-sdk."""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import uuid

HOST = "127.0.0.1"
PORT = 9999


class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        print(f"[helloworld] {fmt % args}")

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length)
        try:
            rpc = json.loads(raw)
        except json.JSONDecodeError:
            self._send(
                400,
                {
                    "jsonrpc": "2.0",
                    "id": None,
                    "error": {"code": -32700, "message": "Parse error"},
                },
            )
            return

        req_id = rpc.get("id")
        method = rpc.get("method")
        params = rpc.get("params") or {}

        if method == "SendMessage":
            message = params.get("message") or {}
            texts = [
                part.get("text")
                for part in message.get("parts") or []
                if isinstance(part, dict) and "text" in part
            ]
            print(
                f"[helloworld] SendMessage messageId={message.get('messageId')} text={texts}"
            )
            result = {
                "id": f"task-{uuid.uuid4().hex[:8]}",
                "contextId": f"ctx-{uuid.uuid4().hex[:8]}",
                "status": {"state": "completed"},
                "history": [
                    {"role": "ROLE_AGENT", "parts": [{"text": "Hello World"}]}
                ],
            }
            self._send(200, {"jsonrpc": "2.0", "id": req_id, "result": result})
            return

        if method == "CancelTask":
            task_id = params.get("id")
            print(f"[helloworld] CancelTask id={task_id}")
            self._send(
                200,
                {
                    "jsonrpc": "2.0",
                    "id": req_id,
                    "result": {
                        "id": task_id,
                        "status": {"state": "canceled"},
                    },
                },
            )
            return

        self._send(
            200,
            {
                "jsonrpc": "2.0",
                "id": req_id,
                "error": {"code": -32601, "message": f"Method not found: {method}"},
            },
        )

    def _send(self, status, body):
        payload = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


if __name__ == "__main__":
    server = ThreadingHTTPServer((HOST, PORT), Handler)
    print(f"[helloworld] A2A JSON-RPC on http://{HOST}:{PORT}/")
    server.serve_forever()
