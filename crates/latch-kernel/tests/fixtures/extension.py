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
        send({"jsonrpc": "2.0", "id": 103, "method": "hook.guard", "params": {"action": "tool.execute"}})
        send({"jsonrpc": "2.0", "id": 104, "method": "hook.transform", "params": {"structure": "model_request"}})
        send({"jsonrpc": "2.0", "id": 105, "method": "context_source.register", "params": {"name": "fixture.context"}})
        send({"jsonrpc": "2.0", "method": "ready", "params": {}})
    elif method == "tool.execute":
        arguments = message["params"]["arguments"]
        if "read_path" in arguments:
            try:
                with open(arguments["read_path"], "rb") as source:
                    source.read(1)
                readable = True
            except OSError:
                readable = False
            send({"jsonrpc": "2.0", "id": message["id"], "result": {"readable": readable}})
        else:
            send({"jsonrpc": "2.0", "id": message["id"], "result": {"echoed": arguments["value"]}})
    elif method == "hook.guard":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"decision": "allow"}})
    elif method == "hook.transform":
        send({"jsonrpc": "2.0", "id": message["id"], "result": message["params"]["value"]})
    elif method == "context_source.get":
        send({"jsonrpc": "2.0", "id": message["id"], "result": {"name": "fixture.context", "content": "fixture context"}})
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": message["id"], "result": None})
    elif method == "exit":
        break
