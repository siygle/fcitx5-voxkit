#!/usr/bin/env python3
"""Fake voxkit daemon for tests: speaks the real line-JSON protocol.

On `stop` it replies with a `final` whose text is FAKE_TEXT (or "discarded"
when FAKE_TEXT is empty). Prints the received requests to the log file.
"""
import json, os, socket, sys

path, text, log = sys.argv[1], sys.argv[2], sys.argv[3]
try:
    os.unlink(path)
except FileNotFoundError:
    pass
srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(path)
os.chmod(path, 0o600)
srv.listen(4)
open(path + ".ready", "w").close()
with open(log, "a", buffering=1) as out:
    while True:
        conn, _ = srv.accept()
        buf = b""
        while True:
            data = conn.recv(4096)
            if not data:
                break
            buf += data
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                msg = json.loads(line)
                out.write(json.dumps(msg, ensure_ascii=False) + "\n")
                req = msg.get("req", 0)
                if msg["type"] == "start":
                    conn.sendall(json.dumps({"v": 1, "type": "ready", "req": req}).encode() + b"\n")
                    conn.sendall(json.dumps({"v": 1, "type": "level", "req": req, "rms_dbfs": -20.0}).encode() + b"\n")
                elif msg["type"] == "stop":
                    reply = {"v": 1, "type": "final", "req": req, "text": text} if text else \
                            {"v": 1, "type": "discarded", "req": req, "reason": "no_speech"}
                    conn.sendall(json.dumps(reply, ensure_ascii=False).encode() + b"\n")
        conn.close()
