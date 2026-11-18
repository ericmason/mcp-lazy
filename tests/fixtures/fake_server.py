"""A scriptable MCP server for mcp-lazy's integration tests.

Environment:
  FAKE_LOG    file to append events to: "start <pid>", "init <params json>", "recv <method>", "child <pid>"
  FAKE_TOOLS  comma-separated tool names to list (default "echo")
  FAKE_START_DELAY  seconds to wait before reading stdin (default 0)
  FAKE_LINGER  ignore SIGTERM; after stdin closes, wait this many seconds,
               write a stray notifications/message, then exit
  FAKE_HEARTBEAT  send a notifications/message every this many seconds
  FAKE_HEARTBEAT_METHOD  send this notification method instead, with no params

Tools (any listed name works for tools/call):
  echo               returns arguments.text, after sleeping arguments.sleep seconds
  crash              exits immediately without answering
  ask_roots          sends roots/list to the client and returns its reply as text
  ask_then_cancel    sends roots/list, cancels it, and answers without waiting
  spawn_child        starts `sleep 300` and logs its pid
  emit_list_changed  sends notifications/tools/list_changed, then answers
  stop_reading       answers, then sleeps without ever reading stdin again
  huge_line          sends a notifications/huge longer than 64 MiB, then answers
"""
import json, os, signal, subprocess, sys, threading, time

LOG = os.environ.get("FAKE_LOG")
TOOLS = os.environ.get("FAKE_TOOLS", "echo").split(",")
LINGER = os.environ.get("FAKE_LINGER")
if LINGER:
    signal.signal(signal.SIGTERM, signal.SIG_IGN)


def log(s):
    if LOG:
        with open(LOG, "a") as f:
            f.write(s + "\n")


out_lock = threading.Lock()


def send(m):
    with out_lock:
        sys.stdout.write(json.dumps(m) + "\n")
        sys.stdout.flush()


HEARTBEAT = os.environ.get("FAKE_HEARTBEAT")
if HEARTBEAT:
    def beat():
        while True:
            time.sleep(float(HEARTBEAT))
            method = os.environ.get("FAKE_HEARTBEAT_METHOD")
            if method:
                send({"jsonrpc": "2.0", "method": method})
            else:
                send({"jsonrpc": "2.0", "method": "notifications/message", "params": {"level": "debug", "data": "beat"}})
    threading.Thread(target=beat, daemon=True).start()


def result(i, r):
    send({"jsonrpc": "2.0", "id": i, "result": r})


def text(i, t):
    result(i, {"content": [{"type": "text", "text": t}]})


deferred = []


def messages():
    while True:
        if deferred:
            yield deferred.pop(0)
            continue
        line = sys.stdin.readline()
        if not line:
            return
        if line.strip():
            yield json.loads(line)


log("start %d" % os.getpid())
time.sleep(float(os.environ.get("FAKE_START_DELAY", "0")))
for m in messages():
    method = m.get("method")
    i = m.get("id")
    log("recv %s" % (method or "response"))
    if method == "initialize":
        log("init " + json.dumps(m.get("params"), sort_keys=True))
        result(i, {
            "protocolVersion": m["params"]["protocolVersion"],
            "capabilities": {"tools": {"listChanged": True}},
            "serverInfo": {"name": "fake", "version": "1"},
        })
    elif method == "tools/list":
        result(i, {"tools": [{"name": n, "description": "d", "inputSchema": {"type": "object"}} for n in TOOLS]})
    elif method == "ping":
        result(i, {})
    elif method == "tools/call":
        name = m["params"]["name"]
        args = m["params"].get("arguments") or {}
        if name == "crash":
            os._exit(3)
        elif name == "ask_roots":
            send({"jsonrpc": "2.0", "id": "srv-1", "method": "roots/list"})
            # Other messages can arrive first; answer them when we get back to the main loop.
            while True:
                other = json.loads(sys.stdin.readline())
                if other.get("id") == "srv-1" and "method" not in other:
                    break
                deferred.append(other)
            log("recv response")
            text(i, json.dumps(other.get("result")))
        elif name == "ask_then_cancel":
            send({"jsonrpc": "2.0", "id": "srv-2", "method": "roots/list"})
            send({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": "srv-2"}})
            text(i, "gave up")
        elif name == "spawn_child":
            c = subprocess.Popen(["sleep", "300"])
            log("child %d" % c.pid)
            text(i, "ok")
        elif name == "emit_list_changed":
            send({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
            text(i, "ok")
        elif name == "stop_reading":
            text(i, "ok")
            time.sleep(3600)
        elif name == "huge_line":
            send({"jsonrpc": "2.0", "method": "notifications/huge", "params": {"pad": "x" * (65 << 20)}})
            text(i, "ok")
        else:
            time.sleep(float(args.get("sleep", 0)))
            text(i, args.get("text", ""))
    elif i is not None:
        send({"jsonrpc": "2.0", "id": i, "error": {"code": -32601, "message": "Method not found"}})

if LINGER:
    time.sleep(float(LINGER))
    send({"jsonrpc": "2.0", "method": "notifications/message", "params": {"level": "info", "data": "old server"}})
