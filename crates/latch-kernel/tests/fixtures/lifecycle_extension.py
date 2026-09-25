#!/usr/bin/env python3
"""Fake extension for extension lifecycle boundedness tests.

Usage: lifecycle_extension.py <mode> [marker-file]

Modes:
  silent-initialize   never answer initialize
  silent-ready        answer initialize, never register or send ready
  silent-rpc          complete the handshake, never answer tool.execute
  silent-shutdown     complete the handshake, never answer shutdown
  ignore-exit         complete the handshake, answer shutdown, never exit
  healthy             complete the handshake and answer requests

The marker path is unique per test; the test scans host `/proc` command lines
for it to observe the sandbox and extension PIDs while the operation runs and
to prove they are gone afterwards. The fake also writes the file at startup as
a liveness record.
"""
import json
import sys
import time

MODE = sys.argv[1] if len(sys.argv) > 1 else "healthy"
MARKER = sys.argv[2] if len(sys.argv) > 2 else ""


def record_started():
    if MARKER:
        with open(MARKER, "w") as out:
            out.write("started\n")


def read_message():
    length = None
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            raise EOFError()
        if line in (b"\n", b"\r\n"):
            break
        name, value = line.decode("ascii").split(":", 1)
        if name.lower() == "content-length":
            length = int(value.strip())
    return json.loads(sys.stdin.buffer.read(length))


def send(message):
    body = json.dumps(message, separators=(",", ":")).encode()
    sys.stdout.buffer.write(f"Content-Length: {len(body)}\r\n\r\n".encode())
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()


def register():
    send(
        {
            "jsonrpc": "2.0",
            "id": 100,
            "method": "tool.register",
            "params": {
                "name": "lifecycle.echo",
                "description": "echo",
                "inputSchema": {"type": "object"},
            },
        }
    )
    send({"jsonrpc": "2.0", "method": "ready", "params": {}})


record_started()
while True:
    try:
        message = read_message()
    except EOFError:
        break
    method = message.get("method")
    if method == "initialize":
        if MODE != "silent-initialize":
            send(
                {
                    "jsonrpc": "2.0",
                    "id": message["id"],
                    "result": {"protocolVersion": "0.1"},
                }
            )
    elif method == "initialized":
        if MODE != "silent-ready":
            register()
    elif method == "tool.execute":
        if MODE != "silent-rpc":
            send(
                {
                    "jsonrpc": "2.0",
                    "id": message["id"],
                    "result": {"echoed": message["params"]["arguments"].get("value")},
                }
            )
    elif method == "shutdown":
        if MODE != "silent-shutdown":
            send({"jsonrpc": "2.0", "id": message["id"], "result": None})
    elif method == "exit":
        if MODE == "ignore-exit":
            while True:
                time.sleep(1)
        break
