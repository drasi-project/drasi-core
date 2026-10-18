"""Local observation endpoint for the native webhook lesson; Python 3 only."""

from http.server import BaseHTTPRequestHandler, HTTPServer
import json


class Receiver(BaseHTTPRequestHandler):
    def do_POST(self):
        if self.path != "/reorders":
            self.send_error(404, "Use /reorders")
            return
        try:
            size = int(self.headers.get("Content-Length", "0"))
        except ValueError:
            self.send_error(400, "Invalid Content-Length")
            return
        if not 0 < size <= 1024 * 1024:
            self.send_error(413, "Expected a bounded JSON body")
            return
        try:
            body = json.loads(self.rfile.read(size))
        except (ValueError, UnicodeDecodeError):
            self.send_error(400, "Invalid JSON")
            return
        print(json.dumps(body, sort_keys=True), flush=True)
        self.send_response(200)
        self.send_header("Content-Length", "0")
        self.end_headers()


if __name__ == "__main__":
    with HTTPServer(("127.0.0.1", 9082), Receiver) as server:
        server.timeout = 1
        print("Observing webhooks at http://127.0.0.1:9082/reorders; Ctrl-C stops.", flush=True)
        try:
            server.serve_forever()
        except KeyboardInterrupt:
            pass
