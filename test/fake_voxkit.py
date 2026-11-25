#!/usr/bin/env python3
"""Fake voxkit daemon for tests: speaks the real line-JSON protocol.

On `stop` it replies with a `final` whose text is FAKE_TEXT (or "discarded"
when FAKE_TEXT is empty). Prints the received requests to the log file.

Translation: `model.status` advertises the "translate" capability unless the
file SOCKET.nocaps exists (an older daemon). A `start` with mode "translate"
ends in a translated final ("[target] " + FAKE_TRANSLATION). While
SOCKET.nocaps exists the connection is closed after each final, so the next
key press reconnects and asks for the capabilities again.
"""
import json, os, socket, sys

path, text, log = sys.argv[1], sys.argv[2], sys.argv[3]
translation = "Nice weather today"
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
        mode, target = "dictate", "en"
        closing = False
        while not closing:
            data = conn.recv(4096)
            if not data:
                break
            buf += data
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                msg = json.loads(line)
                out.write(json.dumps(msg, ensure_ascii=False) + "\n")
                req = msg.get("req", 0)
                if msg["type"] == "model.status":
                    status = {"v": 1, "type": "model.status", "req": req, "daemon": "running"}
                    if not os.path.exists(path + ".nocaps"):
                        status["capabilities"] = ["translate"]
                    conn.sendall(json.dumps(status).encode() + b"\n")
                elif msg["type"] == "start":
                    mode = msg.get("mode", "dictate")
                    target = msg.get("target", "en")
                    conn.sendall(json.dumps({"v": 1, "type": "ready", "req": req}).encode() + b"\n")
                    conn.sendall(json.dumps({"v": 1, "type": "level", "req": req, "rms_dbfs": -20.0}).encode() + b"\n")
                elif msg["type"] == "stop":
                    reply = {"v": 1, "type": "final", "req": req, "text": text} if text else \
                            {"v": 1, "type": "discarded", "req": req, "reason": "no_speech"}
                    if text and mode == "translate":
                        reply.update(text="[%s] %s" % (target, translation), mode="translate",
                                     target=target, translated=True, source=text)
                    conn.sendall(json.dumps(reply, ensure_ascii=False).encode() + b"\n")
                    if os.path.exists(path + ".nocaps"):
                        closing = True
                        break
        conn.close()
