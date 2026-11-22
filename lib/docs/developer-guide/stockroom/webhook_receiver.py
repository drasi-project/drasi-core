"""Local observation endpoint for the native webhook lesson; Python 3 only.

Run with --reject to answer every request with 400 Bad Request instead.
"""

import argparse
from http.server import BaseHTTPRequestHandler, HTTPServer
import json


class Receiver(BaseHTTPRequestHandler):
    reject = False

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
        if self.reject:
            print("Rejected a change with 400 Bad Request.", flush=True)
            # A short plain-text body keeps the sink's error message readable.
            message = b"rejected by --reject"
            self.send_response(400)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", str(len(message)))
            self.end_headers()
            self.wfile.write(message)
            return
        print(json.dumps(body, sort_keys=True), flush=True)
        self.send_response(200)
        self.send_header("Content-Length", "0")
        self.end_headers()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--reject", action="store_true", help="answer every request with 400")
    Receiver.reject = parser.parse_args().reject
    with HTTPServer(("127.0.0.1", 9082), Receiver) as server:
        server.timeout = 1
        mode = "rejecting" if Receiver.reject else "observing"
        print(f"{mode.capitalize()} webhooks at http://127.0.0.1:9082/reorders; Ctrl-C stops.", flush=True)
        try:
            server.serve_forever()
        except KeyboardInterrupt:
            pass
