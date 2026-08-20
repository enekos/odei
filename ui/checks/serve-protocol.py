#!/usr/bin/env python3
"""End-to-end check of `odei serve` against a mock Kimi endpoint.

The Rust unit tests cover the pieces; this covers the loop — a full turn with
a tool call and an approval, then an interrupt landing mid-stream. Both are
places where stdin races the agent thread, which nothing else exercises.

    python3 ui/checks/serve-protocol.py [path/to/odei]
"""
import json
import os
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, HTTPServer

BIN = sys.argv[1] if len(sys.argv) > 1 else os.path.expanduser("~/.cargo/bin/odei")

failures = []


def check(condition, label):
    print(("  ok   " if condition else "  FAIL ") + label)
    if not condition:
        failures.append(label)


def frame(e):
    return ("event: %s\ndata: %s\n\n" % (e["type"], json.dumps(e))).encode()


def spawn(handler_class, permissions="auto"):
    """A mock endpoint plus a serve process wired to it."""
    server = HTTPServer(("127.0.0.1", 0), handler_class)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    workspace = tempfile.mkdtemp(prefix="odei-check-")
    env = dict(
        os.environ,
        HOME=tempfile.mkdtemp(prefix="odei-home-"),
        KIMI_API_KEY="test-key",
        ODEI_BASE_URL="http://127.0.0.1:%d" % server.server_address[1],
        ODEI_PERMISSIONS=permissions,
    )
    proc = subprocess.Popen(
        [BIN, "serve", "--workspace", workspace],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        env=env, text=True, bufsize=1,
    )
    return proc


def send(proc, obj):
    proc.stdin.write(json.dumps(obj) + "\n")
    proc.stdin.flush()


# ------------------------------------------------------- a turn with a tool

TURN1 = b"".join(map(frame, [
    {"type": "message_start", "message": {"usage": {"input_tokens": 1200}}},
    {"type": "content_block_start", "index": 0, "content_block": {"type": "text"}},
    {"type": "content_block_delta", "index": 0,
     "delta": {"type": "text_delta", "text": "Let me look. "}},
    {"type": "content_block_delta", "index": 0,
     "delta": {"type": "text_delta", "text": "Running it now."}},
    {"type": "content_block_stop", "index": 0},
    {"type": "content_block_start", "index": 1,
     "content_block": {"type": "tool_use", "id": "tu_1", "name": "terminal"}},
    {"type": "content_block_delta", "index": 1,
     "delta": {"type": "input_json_delta",
               "partial_json": json.dumps({"action": "exec", "command": "echo hello-from-mock"})}},
    {"type": "content_block_stop", "index": 1},
    {"type": "message_delta", "delta": {"stop_reason": "tool_use"},
     "usage": {"output_tokens": 40}},
    {"type": "message_stop"},
]))

TURN2 = b"".join(map(frame, [
    {"type": "message_start", "message": {"usage": {"input_tokens": 1400}}},
    {"type": "content_block_start", "index": 0, "content_block": {"type": "text"}},
    {"type": "content_block_delta", "index": 0,
     "delta": {"type": "text_delta", "text": "It printed hello-from-mock."}},
    {"type": "content_block_stop", "index": 0},
    {"type": "message_delta", "delta": {"stop_reason": "end_turn"},
     "usage": {"output_tokens": 12}},
    {"type": "message_stop"},
]))


def full_turn():
    print("--- a turn: stream, tool call, approval, inspect ---")
    hits = {"n": 0}

    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            self.rfile.read(int(self.headers.get("content-length", 0)))
            hits["n"] += 1
            body = TURN1 if hits["n"] == 1 else TURN2
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *a):
            pass

    # `ask` so the approval round trip is exercised.
    proc = spawn(Handler, permissions="ask")
    events = []
    done = threading.Event()

    def reader():
        for line in proc.stdout:
            line = line.strip()
            if not line:
                continue
            try:
                e = json.loads(line)
            except json.JSONDecodeError:
                check(False, "every stdout line is JSON (got %r)" % line[:80])
                done.set()
                return
            events.append(e)
            if e["event"] == "approval":
                send(proc, {"cmd": "approve", "id": e["id"], "answer": "allow"})
            elif e["event"] == "turn_end":
                send(proc, {"cmd": "calls"})
            elif e["event"] == "calls" and e["items"]:
                send(proc, {"cmd": "call", "n": e["items"][0]["n"]})
            elif e["event"] == "call":
                done.set()
        done.set()

    threading.Thread(target=reader, daemon=True).start()
    time.sleep(0.4)
    send(proc, {"cmd": "prompt", "text": "run echo and tell me what it said"})
    done.wait(timeout=45)
    send(proc, {"cmd": "exit"})
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()

    def find(kind, **match):
        for e in events:
            if e["event"] == kind and all(e.get(k) == v for k, v in match.items()):
                return e
        return None

    check(find("ready") is not None, "ready is announced before anything else")
    check("".join(e["delta"] for e in events if e["event"] == "text")
          == "Let me look. Running it now.It printed hello-from-mock.",
          "text deltas arrive whole and in order")
    check(find("approval", tool="terminal") is not None, "the terminal call asks first")
    done_event = find("tool", phase="done")
    check(done_event is not None, "the tool reports completion")
    check(done_event and done_event.get("call") == 1, "the done line carries handle #1")
    check(find("turn_end", ok=True) is not None, "the turn ends cleanly")

    states = [e for e in events if e["event"] == "state"]
    check(states and states[-1]["context"] > 0, "context fraction is reported")
    check(states and states[-1]["output_tokens"] == 52, "usage accumulates across steps")

    report = find("call")
    check(report and "hello-from-mock" in report["report"], "the report holds the full output")
    check(report and "echo hello-from-mock" in report["report"],
          "the report holds the command that reproduces it")
    check(proc.stderr.read().strip() == "", "nothing but protocol goes to stdout/stderr")


# ------------------------------------------------------- interrupt mid-stream


def cancel_mid_stream():
    print("--- an interrupt landing mid-stream ---")

    class Handler(BaseHTTPRequestHandler):
        def do_POST(self):
            self.rfile.read(int(self.headers.get("content-length", 0)))
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.end_headers()
            self.wfile.write(frame({"type": "message_start",
                                    "message": {"usage": {"input_tokens": 10}}}))
            self.wfile.write(frame({"type": "content_block_start", "index": 0,
                                    "content_block": {"type": "text"}}))
            self.wfile.flush()
            for _ in range(200):
                try:
                    self.wfile.write(frame({"type": "content_block_delta", "index": 0,
                                            "delta": {"type": "text_delta", "text": "tick "}}))
                    self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError):
                    return
                time.sleep(0.1)

        def log_message(self, *a):
            pass

    proc = spawn(Handler)
    events = []
    ended = threading.Event()

    def reader():
        for line in proc.stdout:
            line = line.strip()
            if not line:
                continue
            events.append(json.loads(line))
            if events[-1]["event"] == "turn_end":
                ended.set()
        ended.set()

    threading.Thread(target=reader, daemon=True).start()
    time.sleep(0.4)
    send(proc, {"cmd": "prompt", "text": "stream forever"})
    time.sleep(2.0)
    send(proc, {"cmd": "cancel"})
    at = time.time()
    ended.wait(timeout=15)
    elapsed = time.time() - at

    deltas = [e for e in events if e["event"] == "text"]
    notices = [e["text"] for e in events if e["event"] == "notice"]
    check(len(deltas) > 3, "text was streaming when the cancel arrived (%d deltas)" % len(deltas))
    check(ended.is_set() and elapsed < 3.0, "the cancel takes effect promptly (%.2fs)" % elapsed)
    check("interrupted" in notices, "the interruption is reported")
    check(any(e["event"] == "turn_end" and e.get("ok") for e in events),
          "an interrupted turn is not an error")

    send(proc, {"cmd": "sessions"})
    time.sleep(1.0)
    check(any(e["event"] == "sessions" for e in events), "serve still answers after a cancel")
    send(proc, {"cmd": "exit"})
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()


if not os.path.exists(BIN):
    sys.exit("no odei binary at %s (pass one as an argument)" % BIN)

full_turn()
cancel_mid_stream()
print("\n%d failed" % len(failures))
sys.exit(1 if failures else 0)
