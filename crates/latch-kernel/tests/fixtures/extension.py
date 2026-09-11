#!/usr/bin/env python3
import json
import sys


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


while True:
    message = read_message()
    method = message.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"protocolVersion": "0.1"}})
    elif method == "initialized":
        send({"jsonrpc": "2.0", "id": 101, "method": "tool.register", "params": {"name": "fixture.echo", "description": "echo", "inputSchema": {"type": "object"}}})
        send({"jsonrpc": "2.0", "id": 102, "method": "command.register", "params": {"name": "fixture-about"}})
        send({"jsonrpc": "2.0", "method": "ready", "params": {}})
    elif method == "tool.execute":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"echoed": message["params"]["arguments"]["value"]}})
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": message["id"], "result": None})
    elif method == "exit":
        break
