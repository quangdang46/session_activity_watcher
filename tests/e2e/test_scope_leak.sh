#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
BIN="$ROOT_DIR/target/debug/saw"
TMP_DIR=$(mktemp -d)

cleanup() {
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

build_binary() {
  cargo build --quiet --manifest-path "$ROOT_DIR/Cargo.toml" --bin saw
}

iso_now() {
  python3 - <<'PY'
from datetime import datetime, timezone
print(datetime.now(timezone.utc).replace(microsecond=0).isoformat().replace("+00:00", "Z"))
PY
}

write_scope_fixture() {
  local jsonl_file=$1
  local session_id=$2
  local file_path=$3
  local timestamp=$4

  python3 - "$jsonl_file" "$session_id" "$file_path" "$timestamp" <<'PY'
import json
import sys

jsonl_file, session_id, file_path, timestamp = sys.argv[1:]
records = [
    {
        "ts": timestamp,
        "kind": "session_started",
        "type": "session_started",
        "sessionId": session_id,
    },
    {
        "type": "assistant",
        "timestamp": timestamp,
        "message": {
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 1},
            "content": [
                {
                    "type": "tool_use",
                    "name": "Write",
                    "input": {"file_path": file_path},
                }
            ],
        },
    },
]

with open(jsonl_file, "w", encoding="utf-8") as fh:
    for record in records:
        fh.write(json.dumps(record) + "\n")
PY
}

watch_first_line() {
  local jsonl_file=$1
  local project_dir=$2
  local guard_dir=$3

  python3 - "$BIN" "$jsonl_file" "$project_dir" "$guard_dir" <<'PY'
import select
import subprocess
import sys

bin_path, jsonl_path, project_dir, guard_dir = sys.argv[1:]
proc = subprocess.Popen(
    [
        bin_path,
        "watch",
        "--file",
        jsonl_path,
        "--dir",
        project_dir,
        "--guard",
        guard_dir,
        "--robot",
        "--no-color",
    ],
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
    text=True,
)

try:
    ready, _, _ = select.select([proc.stdout], [], [], 5)
    if not ready:
        stderr = proc.stderr.read()
        raise SystemExit(f"timed out waiting for watch output\n{stderr}")

    line = proc.stdout.readline().rstrip("\n")
    if not line:
        stderr = proc.stderr.read()
        raise SystemExit(f"watch exited without stdout\n{stderr}")

    print(line)
finally:
    if proc.poll() is None:
        proc.terminate()
        try:
            proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=2)
PY
}

assert_scope_leak_alert() {
  local output=$1
  local expected_file=$2

  python3 - "$output" "$expected_file" <<'PY'
import json
import sys

payload = json.loads(sys.argv[1])
expected_file = sys.argv[2]

if payload.get("event") != "alert":
    raise SystemExit(f"expected alert event\n{json.dumps(payload, indent=2)}")
if payload.get("phase") != "SCOPE_LEAKING":
    raise SystemExit(f"expected SCOPE_LEAKING phase\n{json.dumps(payload, indent=2)}")
if f"file={expected_file}" not in (payload.get("message") or ""):
    raise SystemExit(
        f"expected alert message to contain file={expected_file!r}\n{json.dumps(payload, indent=2)}"
    )
if "tighten --guard" not in (payload.get("suggestion") or ""):
    raise SystemExit(f"expected scope leak suggestion\n{json.dumps(payload, indent=2)}")
PY
}

assert_in_scope_status() {
  local output=$1
  local expected_file=$2

  python3 - "$output" "$expected_file" <<'PY'
import json
import sys

payload = json.loads(sys.argv[1])
expected_file = sys.argv[2]

if payload.get("alert"):
    raise SystemExit(f"did not expect alert for in-scope write\n{json.dumps(payload, indent=2)}")
if payload.get("phase") == "SCOPE_LEAKING":
    raise SystemExit(f"did not expect SCOPE_LEAKING phase for in-scope write\n{json.dumps(payload, indent=2)}")
if payload.get("file") != expected_file:
    raise SystemExit(
        f"expected top-level file={expected_file!r}, got {payload.get('file')!r}\n{json.dumps(payload, indent=2)}"
    )
PY
}

run_scope_leak_case() {
  local project_dir="$TMP_DIR/out-of-scope-project"
  local guard_dir="$project_dir/src/auth"
  local violating_file="$project_dir/src/billing/leak.rs"
  local jsonl_file="$TMP_DIR/out-of-scope.jsonl"
  local output

  mkdir -p "$guard_dir" "$(dirname "$violating_file")"
  printf 'pub fn leak() {}\n' > "$violating_file"
  write_scope_fixture "$jsonl_file" "ses-scope-leak" "$violating_file" "$(iso_now)"

  output=$(watch_first_line "$jsonl_file" "$project_dir" "$guard_dir")
  assert_scope_leak_alert "$output" "src/billing/leak.rs"
}

run_in_scope_case() {
  local project_dir="$TMP_DIR/in-scope-project"
  local guard_dir="$project_dir/src/auth"
  local safe_file="$guard_dir/login.rs"
  local jsonl_file="$TMP_DIR/in-scope.jsonl"
  local output

  mkdir -p "$guard_dir"
  printf 'pub fn login() {}\n' > "$safe_file"
  write_scope_fixture "$jsonl_file" "ses-in-scope" "$safe_file" "$(iso_now)"

  output=$(watch_first_line "$jsonl_file" "$project_dir" "$guard_dir")
  assert_in_scope_status "$output" "src/auth/login.rs"
}

build_binary
run_scope_leak_case
run_in_scope_case

echo "ok - scope leak alert emitted for out-of-scope writes and suppressed for guarded writes"
