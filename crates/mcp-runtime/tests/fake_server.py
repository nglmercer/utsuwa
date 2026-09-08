
import json, os, sys

def reply(id, result):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": id, "result": result}) + "\n")
    sys.stdout.flush()

def reply_error(id, code, message):
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": id,
                                 "error": {"code": code, "message": message}}) + "\n")
    sys.stdout.flush()

TOOLS = [
    {"name": "echo", "description": "Echo a message",
     "inputSchema": {"type": "object", "properties": {"message": {"type": "string"}},
                     "required": ["message"]}},
    {"name": "failer", "description": "Always fails at the tool level",
     "inputSchema": {"type": "object"}},
    {"name": "envcheck", "description": "Report environment visibility",
     "inputSchema": {"type": "object"}},
]

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except Exception:
        continue
    method = msg.get("method", "")
    id = msg.get("id")
    if method == "initialize":
        version = (msg.get("params") or {}).get("protocolVersion", "2024-11-05")
        reply(id, {"protocolVersion": version, "capabilities": {"tools": {}},
                   "serverInfo": {"name": "fake", "version": "0.0.1"}})
    elif method == "tools/list":
        reply(id, {"tools": TOOLS})
    elif method == "tools/call":
        params = msg.get("params") or {}
        name = params.get("name")
        args = params.get("arguments") or {}
        if name == "echo":
            reply(id, {"content": [{"type": "text",
                                    "text": "echo:" + str(args.get("message", ""))}]})
        elif name == "failer":
            reply(id, {"content": [{"type": "text", "text": "boom"}],
                       "isError": True})
        elif name == "envcheck":
            reply(id, {"content": [{"type": "text",
                                    "text": "secret=%s;extra=%s" % (
                                        os.environ.get("UTSUWA_MCP_PROBE_SECRET", "absent"),
                                        os.environ.get("UTSUWA_MCP_PROBE_EXTRA", "absent"))}]})
        else:
            reply_error(id, -32602, "unknown tool " + str(name))
    elif id is not None and method not in ("notifications/initialized",):
        reply_error(id, -32601, "unknown method " + method)

