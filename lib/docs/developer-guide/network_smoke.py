"""Exercise the book's actual Stockroom binary and native network libraries."""

import argparse
from contextlib import contextmanager
import http.client
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
import os
from pathlib import Path
import queue
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time


BOOK = Path(__file__).resolve().parent
DEADLINE = 15


def row(quantity):
    return {"sku": "bolts", "quantity": quantity}


@contextmanager
def refused_endpoint(port=0):
    with socket.socket() as reservation:
        reservation.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        reservation.bind(("127.0.0.1", port))
        # Keep the port owned without listening: requests must fail, never reach another service.
        yield f"http://127.0.0.1:{reservation.getsockname()[1]}/reorders"


class Stockroom:
    def __init__(self, binary, arguments):
        self.binary = binary
        self.arguments = arguments

    def __enter__(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="stockroom-network-")
        self.directory = Path(self.temporary.name)
        self.save_inventory(8)
        self.save_destination(None)
        self.stdout = self.directory / "stdout.log"
        self.stderr = self.directory / "stderr.log"
        environment = dict(os.environ, RUST_LOG="off", NO_PROXY="127.0.0.1,localhost")
        try:
            with self.stdout.open("w") as out, self.stderr.open("w") as err:
                self.process = subprocess.Popen(
                    [str(self.binary), *map(str, self.arguments)],
                    cwd=self.directory, env=environment, stdin=subprocess.DEVNULL,
                    stdout=out, stderr=err,
                )
        except BaseException:
            self.temporary.cleanup()
            raise
        return self

    def __exit__(self, kind, error, traceback):
        try:
            if self.process.poll() is None:
                self.process.kill()
                self.process.wait(timeout=DEADLINE)
            if kind is not None:
                print(self.diagnostics(), file=sys.stderr)
        finally:
            self.temporary.cleanup()

    def diagnostics(self):
        return f"Stockroom stdout:\n{self.stdout.read_text()}\nStockroom stderr:\n{self.stderr.read_text()}"

    def save_inventory(self, quantity):
        inventory = json.loads((BOOK / "stockroom/inventory.json").read_text())
        inventory[0]["quantity"] = quantity
        self.save("inventory.json", inventory)

    def save_destination(self, url):
        self.save("destination.json", {"webhook": url})

    def save(self, filename, value):
        staged = self.directory / f"{filename}.new"
        staged.write_text(json.dumps(value))
        staged.replace(self.directory / filename)

    def wait_output(self, expected):
        deadline = time.monotonic() + DEADLINE
        while time.monotonic() < deadline:
            if expected in self.stdout.read_text():
                self.alive()
                return
            if self.process.poll() is not None:
                raise AssertionError(f"Stockroom exited before {expected!r}\n{self.diagnostics()}")
            time.sleep(0.02)
        raise AssertionError(f"Timed out waiting for {expected!r}\n{self.diagnostics()}")

    def alive(self):
        assert self.process.poll() is None, self.diagnostics()

    def stop(self):
        self.alive()
        self.process.send_signal(signal.SIGINT)
        assert self.process.wait(timeout=DEADLINE) == 0, self.diagnostics()

    def expect_failure(self, *messages):
        assert self.process.wait(timeout=DEADLINE) != 0, self.diagnostics()
        error = self.stderr.read_text()
        for message in messages:
            assert message in error, self.diagnostics()


class Receiver:
    def __enter__(self):
        self.messages = queue.Queue()
        messages = self.messages

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                try:
                    assert self.path == "/reorders", self.path
                    length = int(self.headers["Content-Length"])
                    assert 0 < length <= 1024 * 1024, length
                    self.connection.settimeout(DEADLINE)
                    body = json.loads(self.rfile.read(length))
                except (AssertionError, KeyError, ValueError, OSError) as error:
                    messages.put(error)
                    self.send_error(400)
                    return
                self.send_response(200)
                self.send_header("Content-Length", "0")
                self.end_headers()
                messages.put(body)

            def log_message(self, *arguments):
                pass

        self.server = HTTPServer(("127.0.0.1", 0), Handler)
        self.port = self.server.server_port
        self.url = f"http://127.0.0.1:{self.port}/reorders"
        self.thread = threading.Thread(
            target=self.server.serve_forever, kwargs={"poll_interval": 0.02}
        )
        self.thread.start()
        self.closed = False
        return self

    def close(self):
        if not self.closed:
            self.server.shutdown()
            self.server.server_close()
            self.thread.join(timeout=DEADLINE)
            assert not self.thread.is_alive(), "webhook receiver did not stop"
            self.closed = True

    def __exit__(self, *arguments):
        self.close()

    def receive(self, operation, before=None, after=None):
        message = self.messages.get(timeout=DEADLINE)
        if isinstance(message, Exception):
            raise message
        assert message["output"] == LOW_STOCK_OUTPUT, message
        assert message["operation"] == operation, message
        assert message.get("before") == before, message
        assert message.get("after") == after, message
        assert isinstance(message["identity"]["id"], str), message
        assert isinstance(message["sequence"], int), message
        assert isinstance(message["timestamp"], str), message
        for stale in ("queryId", "sequenceId", "row_signature", "metadata"):
            assert stale not in message, message
        return message


LOW_STOCK_OUTPUT = {"alias": "low-stock", "component": "low-stock", "port": "out"}


def post_event(port, event):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=DEADLINE)
    try:
        connection.request(
            "POST", "/sources/inventory/events", json.dumps(event),
            {"Content-Type": "application/json"},
        )
        response = connection.getresponse()
        body = response.read()
        assert response.status == 200, (response.status, body)
    finally:
        connection.close()


def sse_result(response, operation, quantity):
    deadline = time.monotonic() + DEADLINE
    data = []
    while time.monotonic() < deadline:
        line = response.readline(65536)
        assert line, "SSE connection ended before the expected result"
        assert len(line) < 65536, "SSE line exceeded the lesson's bound"
        if line.startswith(b"data:"):
            data.append(line[5:].strip())
        elif not line.strip() and data:
            message = json.loads(b"\n".join(data))
            data = []
            if message.get("type") == "heartbeat":
                continue
            assert message["output"] == LOW_STOCK_OUTPUT, message
            assert isinstance(message["publishedAt"], int), message
            assert len(message["changes"]) == 1, message
            change = message["changes"][0]
            assert change["operation"] == operation, change
            image = "before" if operation == "DELETE" else "after"
            assert change[image] == row(quantity), change
            assert isinstance(change["identity"]["id"], str), change
            for stale in ("row_signature", "queryId", "metadata"):
                assert stale not in change, change
            return change
    raise AssertionError("SSE result deadline elapsed")


def http_to_sse(binary, libraries):
    with socket.socket() as http_socket, socket.socket() as sse_socket:
        http_socket.bind(("127.0.0.1", 0))
        sse_socket.bind(("127.0.0.1", 0))
        http_port = http_socket.getsockname()[1]
        sse_port = sse_socket.getsockname()[1]
    with Stockroom(binary, ["plugins", libraries, http_port, sse_port]) as app:
        app.wait_output(f"SSE output at 127.0.0.1:{sse_port}/events.")
        connection = http.client.HTTPConnection("127.0.0.1", sse_port, timeout=DEADLINE)
        try:
            connection.request("GET", "/events")
            response = connection.getresponse()
            assert response.status == 200, response.status
            assert "text/event-stream" in response.getheader("Content-Type", "")
            insert = json.loads((BOOK / "stockroom/http-insert.json").read_text())
            post_event(http_port, insert)
            added = sse_result(response, "ADD", 3)
            update = json.loads((BOOK / "stockroom/http-restock.json").read_text())
            update["element"]["properties"]["quantity"] = 2
            post_event(http_port, update)
            changed = sse_result(response, "REPLACE", 2)
            assert changed["before"] == row(3) and changed["after"] == row(2), changed
            assert changed["identity"] == added["identity"], changed
            post_event(http_port, json.loads((BOOK / "stockroom/http-restock.json").read_text()))
            deleted = sse_result(response, "DELETE", 2)
            assert deleted["identity"] == added["identity"], deleted
            response.close()
        finally:
            connection.close()
        app.stop()


def webhook(binary, libraries):
    with Receiver() as receiver:
        with Stockroom(binary, ["webhook", libraries, receiver.url]) as app:
            app.wait_output("Connected live webhook destination")
            app.save_inventory(3)
            added = receiver.receive("ADD", after=row(3))
            app.save_inventory(2)
            changed = receiver.receive("REPLACE", before=row(3), after=row(2))
            app.save_inventory(8)
            deleted = receiver.receive("DELETE", before=row(2))
            assert added["sequence"] < changed["sequence"] < deleted["sequence"]
            app.stop()


def unreachable_webhook(binary, libraries):
    with refused_endpoint() as url:
        with Stockroom(binary, ["webhook", libraries, url]) as app:
            app.wait_output("Connected live webhook destination")
            app.save_inventory(3)
            app.expect_failure("component 'webhook' failed", "error sending request")
            assert url in app.diagnostics(), app.diagnostics()


def live_destination(binary, libraries):
    with Receiver() as receiver:
        with Stockroom(binary, ["live", libraries]) as app:
            app.wait_output("set a webhook URL in destination.json")
            app.save_inventory(3)
            app.wait_output('low-stock: [{"quantity":3,"sku":"bolts"}]')
            app.save_destination(receiver.url)
            app.wait_output("Connected live webhook destination")
            app.save_inventory(2)
            receiver.receive("REPLACE", before=row(3), after=row(2))
            app.save_inventory(8)
            receiver.receive("DELETE", before=row(2))
            app.wait_output("low-stock: []")
            receiver.close()
            with refused_endpoint(receiver.port) as url:
                app.save_inventory(3)
                app.expect_failure("component 'webhook' failed", "error sending request")
                assert url in app.diagnostics(), app.diagnostics()


def unsupported_destination_changes(binary, libraries):
    with refused_endpoint() as url:
        for replacement in (None, f"{url}/replacement"):
            with Stockroom(binary, ["live", libraries]) as app:
                app.wait_output("set a webhook URL in destination.json")
                app.save_destination(url)
                app.wait_output("Connected live webhook destination")
                app.save_destination(replacement)
                app.expect_failure("this lesson adds one destination")


def run(binary, libraries):
    if os.name != "posix":
        raise SystemExit("Native network smoke tests currently qualify Linux/macOS only.")
    for scenario in (
        http_to_sse, webhook, unreachable_webhook, live_destination,
        unsupported_destination_changes,
    ):
        scenario(binary, libraries)
        print(f"Passed native network lesson: {scenario.__name__}", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--libraries", required=True, type=Path)
    args = parser.parse_args()
    run(args.binary.resolve(strict=True), args.libraries.resolve(strict=True))
