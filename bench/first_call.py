#!/usr/bin/env python3
"""Time the first tools/call in a session, direct versus through mcp-lazy with a warm cache.

Usage: first_call.py MCP_LAZY_BIN CACHE_DIR
"""
import json, subprocess, sys, time

LAZY, CACHE = sys.argv[1], sys.argv[2]
CMD = ["npx", "-y", "@modelcontextprotocol/server-memory"]
INIT = [
    {"jsonrpc": "2.0", "id": 1, "method": "initialize",
     "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "bench", "version": "0"}}},
    {"jsonrpc": "2.0", "method": "notifications/initialized"},
    {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
]
CALL = {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "read_graph", "arguments": {}}}


def until(p, want):
    while True:
        m = json.loads(p.stdout.readline())
        if m.get("id") == want:
            return


def session(cmd):
    p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    t0 = time.time()
    for m in INIT:
        p.stdin.write((json.dumps(m) + "\n").encode())
    p.stdin.flush()
    until(p, 2)
    t1 = time.time()
    p.stdin.write((json.dumps(CALL) + "\n").encode())
    p.stdin.flush()
    until(p, 3)
    t2 = time.time()
    p.stdin.close()
    p.wait()
    return round(t1 - t0, 2), round(t2 - t1, 2)


session([LAZY, "--cache-dir", CACHE, "--"] + CMD)  # warm npx and the cache
for mode, cmd in (("direct", CMD), ("lazy", [LAZY, "--cache-dir", CACHE, "--"] + CMD)):
    runs = [session(cmd) for _ in range(3)]
    print(json.dumps({"mode": mode, "startup_to_tools_list_s": [r[0] for r in runs], "first_call_s": [r[1] for r in runs]}))
