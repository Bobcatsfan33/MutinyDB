#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

smoke_dir="$(mktemp -d)"
server_pid=""
cleanup() {
  if [ -n "$server_pid" ]; then kill "$server_pid" 2>/dev/null || true; fi
  rm -rf "$smoke_dir"
}
trap cleanup EXIT HUP INT TERM

target/debug/mutinyd init "$smoke_dir/mutinydb.json"
if [ "$(uname -s)" = "Darwin" ]; then config_mode="$(stat -f '%Lp' "$smoke_dir/mutinydb.json")"; else config_mode="$(stat -c '%a' "$smoke_dir/mutinydb.json")"; fi
test "$config_mode" = "600"
target/debug/mutinyd "$smoke_dir/mutinydb.json" >"$smoke_dir/server.out" 2>"$smoke_dir/server.err" &
server_pid=$!
for _ in $(seq 1 60); do
  if curl -fsS http://127.0.0.1:7654/v1/agent/health >/dev/null 2>&1; then break; fi
  sleep 0.25
done
curl -fsS http://127.0.0.1:7654/v1/agent/health | grep 'surface v0.1'

PYTHONPATH=sdk/python/src python3 - <<'PY'
from mutinydb import MutinyDB

db = MutinyDB("http://127.0.0.1:7654", "agent")
db.session_open("sdk-smoke")
receipt = db.write(
    actor="sdk-test", session="sdk-smoke", branch="sdk-smoke",
    intent="prove the public client against the real server",
    sources=[{"system": "test", "record": "fixture-1"}], table="memory",
    rows=[["memory-1", "sdk-smoke", "the agent learned the deployment succeeded", 1, False, 1]],
)
assert receipt.commit == 1 and receipt.epoch == 1 and receipt.rows == 1, receipt
handle = db.register("SELECT memory.branch AS branch, COUNT(*) AS memories FROM memory GROUP BY memory.branch")
answer = db.read(handle)
assert answer.epoch == 1 and "sdk-smoke" in answer.body, answer
hits = db.semantic_answer("sdk-smoke", "agent-recall")
assert hits and hits[0].key == "memory-1", hits
groups = db.semantic_groups("sdk-smoke", "agent-themes")
assert groups and "memory-1" in groups[0].member_keys, groups
print("Python SDK end-to-end: green")
PY
