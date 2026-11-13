#!/usr/bin/env python3
"""Time the full daemon path the way the input method uses it.

Connects to a voxkit socket, sends start (with vocabulary), then stop, and
measures release -> final. Repeats N rounds on one connection, then one round
on a fresh connection (fcitx5 restart case).
"""
import json, socket, sys, time

path, rounds = sys.argv[1], int(sys.argv[2])
vocab = sys.argv[3].split("|") if len(sys.argv) > 3 and sys.argv[3] else []


def one(conn, req):
    f = conn.makefile("rwb")
    def send(obj):
        f.write(json.dumps(obj, ensure_ascii=False).encode() + b"\n"); f.flush()
    send({"v": 1, "type": "start", "req": req, "language": "zh",
          "convert": "s2twp", "vocabulary": vocab})
    time.sleep(0.3)  # "speaking"
    t = time.perf_counter()
    send({"v": 1, "type": "stop", "req": req})
    while True:
        msg = json.loads(f.readline())
        if msg.get("req") == req and msg["type"] in ("final", "discarded", "error"):
            return (time.perf_counter() - t) * 1000, msg.get("text", msg["type"])


conn = socket.socket(socket.AF_UNIX); conn.connect(path)
for i in range(rounds):
    ms, text = one(conn, i + 1)
    print(f"round {i + 1}: {ms:6.0f} ms  {text}")
conn.close()
conn = socket.socket(socket.AF_UNIX); conn.connect(path)
ms, text = one(conn, 99)
print(f"reconnect: {ms:6.0f} ms  {text}")
