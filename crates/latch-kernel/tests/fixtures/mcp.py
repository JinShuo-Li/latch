"""Credential-free newline MCP fixture, used only inside the mandatory sandbox."""
import json
import sys
import time

legacy = "--legacy" in sys.argv
for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    if "id" not in request:
        continue
    result = {}
    error = None
    if method == "server/discover":
        if legacy:
            error = {"code": -32602, "message": "initialize first"}
        else:
            result = {"supportedVersions": ["2026-07-28"], "capabilities": {"tools": {}}}
    elif method == "initialize":
        result = {"protocolVersion": "2024-11-05", "capabilities": {"tools": {}}, "serverInfo": {"name": "fixture", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "echo", "description": "fixture echo", "inputSchema": {"type": "object"}, "annotations": {"readOnlyHint": True}}]}
    elif method == "tools/call":
        arguments = request["params"]["arguments"]
        if arguments.get("sleep"):
            time.sleep(2)
        result = {"content": [{"type": "text", "text": json.dumps(arguments)}], "isError": arguments.get("fail", False)}
    else:
        error = {"code": -32601, "message": "unsupported"}
    if not legacy and not error:
        result["resultType"] = "complete"
    response = {"jsonrpc": "2.0", "id": request["id"]}
    response["error" if error else "result"] = error or result
    print(json.dumps(response), flush=True)
