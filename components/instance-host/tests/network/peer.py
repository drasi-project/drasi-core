#!/usr/bin/env python3
# Copyright 2026 The Drasi Authors.
# Licensed under the Apache License, Version 2.0.
"""Bounded, test-only local control and TLS fault injection for a Linux peer."""

import hashlib
import json
import os
from pathlib import Path
import socket
import ssl
import struct
import subprocess
import sys
import time
import uuid

MAX_FRAME = 2 * 1024 * 1024
os.umask(0o077)


def input_json():
    data = sys.stdin.buffer.read(MAX_FRAME + 1)
    if not 0 < len(data) <= MAX_FRAME:
        raise ValueError("test input exceeds frame limit")
    return json.loads(data)


def receive(stream, length):
    result = bytearray()
    while len(result) < length:
        block = stream.recv(length - len(result))
        if not block:
            raise EOFError("peer closed a partial test frame")
        result.extend(block)
    return bytes(result)


def control(state):
    path = Path(state) / "control.json"
    if path.stat().st_size > 8192:
        raise ValueError("oversized control credentials")
    credentials = json.loads(path.read_bytes())
    request_id = str(uuid.uuid4())
    request = {
        key: credentials[key]
        for key in ["version", "instance_id", "generation", "token"]
    }
    request.update(request_id=request_id, command=input_json())
    data = json.dumps(request).encode()
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(2)
        stream.connect(credentials["endpoint"])
        stream.sendall(struct.pack(">I", len(data)) + data)
        length = struct.unpack(">I", receive(stream, 4))[0]
        if not 0 < length <= MAX_FRAME:
            raise ValueError("oversized control response")
        response = json.loads(receive(stream, length))
    for key in ["version", "instance_id", "generation"]:
        if response[key] != credentials[key]:
            raise ValueError("foreign control response")
    if response["request_id"] != request_id or response.get("error"):
        raise ValueError(f"control request failed: {response.get('error')}")
    print(json.dumps(response["reply"]))


def seed(root):
    root = Path(root)
    root.mkdir(mode=0o700, parents=False, exist_ok=True)
    manifest = input_json()
    (root / "host.json").write_text(json.dumps(manifest))
    key = root / "key"
    if not key.exists():
        key.write_bytes(os.urandom(32))


def certificates(root):
    root = Path(root)
    root.mkdir(mode=0o700)

    def openssl(*arguments):
        subprocess.run(
            ["openssl", *arguments], cwd=root, check=True,
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=10,
        )

    (root / "ca.cnf").write_text(
        "[req]\nprompt=no\ndistinguished_name=dn\nx509_extensions=ca\n"
        "[dn]\nCN=Drasi Network Qualification CA\n[ca]\n"
        "basicConstraints=critical,CA:TRUE\n"
        "keyUsage=critical,keyCertSign,cRLSign\nsubjectKeyIdentifier=hash\n"
        "authorityKeyIdentifier=keyid:always\n"
    )
    openssl("req", "-x509", "-newkey", "rsa:2048", "-nodes",
            "-keyout", "ca.key", "-out", "ca.pem", "-days", "1",
            "-sha256", "-config", "ca.cnf")
    openssl("x509", "-in", "ca.pem", "-outform", "DER", "-out", "ca.der")
    for name in ["client", "server"]:
        (root / f"{name}.ext").write_text(
            "basicConstraints=critical,CA:FALSE\n"
            "keyUsage=critical,digitalSignature,keyEncipherment\n"
            "extendedKeyUsage=serverAuth,clientAuth\nsubjectAltName=DNS:localhost\n"
            "subjectKeyIdentifier=hash\nauthorityKeyIdentifier=keyid,issuer\n"
        )
        openssl("req", "-new", "-newkey", "rsa:2048", "-nodes",
                "-keyout", f"{name}.key", "-out", f"{name}.csr",
                "-subj", "/CN=localhost", "-config", "ca.cnf")
        openssl("x509", "-req", "-in", f"{name}.csr", "-CA", "ca.pem",
                "-CAkey", "ca.key", "-CAcreateserial", "-out", f"{name}.pem",
                "-days", "1", "-sha256", "-extfile", f"{name}.ext")
        openssl("x509", "-in", f"{name}.pem", "-outform", "DER",
                "-out", f"{name}.der")
        openssl("pkcs8", "-topk8", "-nocrypt", "-in", f"{name}.key",
                "-outform", "DER", "-out", f"{name}-key.der")


def bridge_frame(stream, lane):
    header = receive(stream, 12)
    length = struct.unpack(">I", header[8:])[0]
    if (header[:8] != b"DRBG\x00\x01" + bytes([lane, 0])
            or not 0 < length <= MAX_FRAME):
        raise ValueError("unexpected bounded bridge frame")
    return header + receive(stream, length)


def proxy(root, address):
    root = Path(root)
    certificates_path = root / "tls"
    server = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    server.load_cert_chain(certificates_path / "server.pem", certificates_path / "server.key")
    server.load_verify_locations(certificates_path / "ca.pem")
    server.verify_mode = ssl.CERT_REQUIRED
    server.set_alpn_protocols(["drasi-bridge/1"])
    client = ssl.create_default_context(cafile=certificates_path / "ca.pem")
    client.load_cert_chain(certificates_path / "client.pem", certificates_path / "client.key")
    client.set_alpn_protocols(["drasi-bridge/1"])
    pins = {
        name: hashlib.sha256((certificates_path / f"{name}.der").read_bytes()).hexdigest()
        for name in ["client", "server"]
    }

    def event(value):
        with (root / "faults.jsonl").open("a") as output:
            output.write(json.dumps(value) + "\n")

    host, port = address.rsplit(":", 1)
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        listener.listen(1)
        listener.settimeout(30)
        (root / "proxy.json").write_text(json.dumps({"port": listener.getsockname()[1]}))
        connection = 0
        while True:
            raw, _ = listener.accept()
            raw.settimeout(10)
            with server.wrap_socket(raw, server_side=True) as incoming:
                if (incoming.selected_alpn_protocol() != "drasi-bridge/1"
                        or hashlib.sha256(incoming.getpeercert(binary_form=True)).hexdigest()
                        != pins["client"]):
                    raise ValueError("unauthorized test-proxy client")
                with socket.create_connection((host, int(port)), timeout=5) as outgoing_raw:
                    with client.wrap_socket(outgoing_raw, server_hostname="localhost") as outgoing:
                        if (outgoing.selected_alpn_protocol() != "drasi-bridge/1"
                                or hashlib.sha256(outgoing.getpeercert(binary_form=True)).hexdigest()
                                != pins["server"]):
                            raise ValueError("unauthorized Linux peer")
                        connection += 1
                        hello = bridge_frame(incoming, 2)
                        if connection == 2:
                            invalid = bytearray(hello)
                            invalid[5] = 2
                            outgoing.sendall(invalid)
                            try:
                                reply = outgoing.recv(12)
                            except (ConnectionResetError, ssl.SSLError):
                                reply = b""
                            if reply.startswith(b"DRBG"):
                                raise ValueError("peer accepted an incompatible wire version")
                            event({"protocol_rejected": True})
                            continue
                        outgoing.sendall(hello)
                        incoming.sendall(bridge_frame(outgoing, 4))
                        while True:
                            try:
                                request = bridge_frame(incoming, 3)
                            except EOFError:
                                break
                            outgoing.sendall(request)
                            reply = bridge_frame(outgoing, 4)
                            if connection == 1:
                                # Named MessagePack enum: this must be a committed
                                # Receipt, not a Ready/Failure or an unobserved write.
                                if not reply[12:].startswith(b"\x81\xa7Receipt"):
                                    raise ValueError("fault injection did not observe an admission receipt")
                                event({"receipt_held": True, "sha256": hashlib.sha256(reply).hexdigest()})
                                deadline = time.monotonic() + 10
                                while not (root / "release").exists():
                                    if time.monotonic() >= deadline:
                                        raise TimeoutError("receipt fault gate was not released")
                                    time.sleep(0.01)
                                event({"receipt_dropped": True})
                                break
                            incoming.sendall(reply)


if __name__ == "__main__":
    mode, path = sys.argv[1:3]
    if mode == "control":
        control(path)
    elif mode == "seed":
        seed(path)
    elif mode == "certificates":
        certificates(path)
    elif mode == "proxy":
        proxy(path, sys.argv[3])
    else:
        raise ValueError("unknown qualification helper command")
