#!/usr/bin/env python3
"""Measure memory and process count for N idle MCP client sessions, with and without mcp-lazy.

Each simulated session starts every server the way Claude Code does (initialize, notifications/initialized,
tools/list), then sits idle. After SETTLE seconds, the script sums the resident memory of every process
under the sessions it started.

Usage: bench.py MCP_LAZY_BIN SESSIONS SETTLE_SECONDS
"""
import json, os, subprocess, sys, tempfile, time

LAZY, N, SETTLE = sys.argv[1], int(sys.argv[2]), float(sys.argv[3])
SERVERS = {
    "playwright-mcp": ["npx", "-y", "@playwright/mcp@latest", "--headless", "--isolated"],
    "chrome-devtools-mcp": ["npx", "-y", "chrome-devtools-mcp@latest", "--headless", "--isolated"],
    "server-filesystem": ["npx", "-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
    "server-memory": ["npx", "-y", "@modelcontextprotocol/server-memory"],
}
MSGS = [
    {"jsonrpc": "2.0", "id": 1, "method": "initialize",
     "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "bench", "version": "0"}}},
    {"jsonrpc": "2.0", "method": "notifications/initialized"},
    {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
]


def tree_stats(roots):
    out = subprocess.run(["ps", "-axo", "pid=,ppid=,rss="], capture_output=True, text=True).stdout
    kids, rss = {}, {}
    for line in out.split("\n"):
        if line.strip():
            pid, ppid, r = map(int, line.split())
            kids.setdefault(ppid, []).append(pid)
            rss[pid] = r
    seen, stack = set(), list(roots)
    while stack:
        p = stack.pop()
        if p in seen or p not in rss:
            continue
        seen.add(p)
        stack.extend(kids.get(p, []))
    return len(seen), sum(rss[p] for p in seen) / 1024


def start(cmd):
    p = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    for m in MSGS:
        p.stdin.write((json.dumps(m) + "\n").encode())
    p.stdin.flush()
    return p


def wait_tools(p, timeout=120):
    end = time.time() + timeout
    while time.time() < end:
        line = p.stdout.readline()
        if not line:
            return None
        try:
            m = json.loads(line)
        except ValueError:
            continue
        if m.get("id") == 2:
            return len(m.get("result", {}).get("tools", []))
    return None


def run(mode, cache):
    procs = []
    for _ in range(N):
        for name, cmd in SERVERS.items():
            full = cmd if mode == "direct" else [LAZY, "--cache-dir", cache, "--"] + cmd
            procs.append((name, start(full)))
    tools = {}
    for name, p in procs:
        tools.setdefault(name, set()).add(wait_tools(p))
    time.sleep(SETTLE)
    count, mb = tree_stats([p.pid for _, p in procs])
    for _, p in procs:
        p.stdin.close()
    for _, p in procs:
        try:
            p.wait(timeout=10)
        except subprocess.TimeoutExpired:
            p.kill()
    return count, mb, tools


cache = tempfile.mkdtemp(prefix="mcp-lazy-bench-")
# Warm the npx cache and the mcp-lazy cache once, the way the first session ever would.
N_REAL, N = N, 1
run("lazy", cache)
N = N_REAL
for mode in ("direct", "lazy"):
    count, mb, tools = run(mode, cache)
    print(json.dumps({"mode": mode, "sessions": N, "servers": len(SERVERS), "processes": count,
                      "rss_mb": round(mb), "tools": {k: sorted(x for x in v if x is not None) for k, v in tools.items()}}))
