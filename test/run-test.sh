#!/usr/bin/env bash
# Isolated run: temp HOME/XDG dirs, a fake daemon on a private socket.
set -euo pipefail
build=$1
here=$(cd "$(dirname "$0")" && pwd)
root=$(mktemp -d)
trap 'kill $fake 2>/dev/null || true; rm -rf "$root"' EXIT
text="今天天氣很好"
python3 -I "$here/fake_voxkit.py" "$root/voxkit.sock" "$text" "$root/fake.log" &
fake=$!
for _ in $(seq 50); do [[ -e $root/voxkit.sock.ready ]] && break; sleep 0.05; done
env -i PATH="$PATH" HOME="$root" XDG_CONFIG_HOME="$root/config" \
  XDG_DATA_HOME="$root/data" XDG_RUNTIME_DIR="$root" \
  VOXKIT_SOCKET="$root/voxkit.sock" FAKE_TEXT="$text" FAKE_LOG="$root/fake.log" \
  "$build/test/testvoxkit"
