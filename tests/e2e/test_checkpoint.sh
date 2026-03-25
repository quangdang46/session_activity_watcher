#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
BIN="$ROOT_DIR/target/debug/saw"
TMP_DIR=$(mktemp -d)
CHECKPOINT_PID=""

cleanup() {
  set +e

  if [[ -n "$CHECKPOINT_PID" ]] && kill -0 "$CHECKPOINT_PID" 2>/dev/null; then
    kill -CONT "$CHECKPOINT_PID" 2>/dev/null
    kill "$CHECKPOINT_PID" 2>/dev/null
  fi

  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

fail() {
  printf '%s\n' "$1" >&2
  exit 1
}

build_binary() {
  cargo build --quiet --manifest-path "$ROOT_DIR/Cargo.toml" --bin saw
}

write_checkpoint_fixture() {
  local jsonl_file=$1
  local session_id=$2
  local file_one=$3
  local file_two=$4

  python3 - "$jsonl_file" "$session_id" "$file_one" "$file_two" <<'PY'
import json
import sys
from datetime import datetime, timedelta, timezone

jsonl_file, session_id, file_one, file_two = sys.argv[1:]
now = datetime.now(timezone.utc).replace(microsecond=0)
records = [
    {
        "type": "session_started",
        "timestamp": (now - timedelta(seconds=123)).isoformat().replace("+00:00", "Z"),
        "sessionId": session_id,
    },
    {
        "type": "assistant",
        "timestamp": (now - timedelta(seconds=122)).isoformat().replace("+00:00", "Z"),
        "message": {
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 1},
            "content": [
                {
                    "type": "tool_use",
                    "name": "Write",
                    "input": {"file_path": file_one},
                }
            ],
        },
    },
    {
        "type": "assistant",
        "timestamp": (now - timedelta(seconds=121)).isoformat().replace("+00:00", "Z"),
        "message": {
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 1},
            "content": [
                {
                    "type": "tool_use",
                    "name": "Write",
                    "input": {"file_path": file_two},
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

run_watch_capture() {
  local project_dir=$1
  local pid=$2
  local jsonl_file=$3
  local mode=$4
  local output
  local status

  set +e
  output=$(timeout 5 "$BIN" watch --file "$jsonl_file" --pid "$pid" --dir "$project_dir" --timeout-secs 120 $mode --robot --no-color 2>&1)
  status=$?
  set -e

  if [[ $status -ne 0 && $status -ne 124 ]]; then
    printf 'watch command failed with status %s\n%s\n' "$status" "$output" >&2
    return 1
  fi

  printf '%s' "$output"
}

assert_checkpoint_alert() {
  local output=$1
  local project_dir=$2
  local expected_action=$3

  python3 - "$output" "$project_dir" "$expected_action" <<'PY'
import json
import os
import sys

output, project_dir, expected_action = sys.argv[1:]
payloads = []
for line in output.splitlines():
    line = line.strip()
    if not line or not line.startswith("{"):
        continue
    try:
        payloads.append(json.loads(line))
    except json.JSONDecodeError:
        continue

if not payloads:
    raise SystemExit("expected watch output")

alert = next((payload for payload in payloads if payload.get("event") == "alert"), None)
if alert is None:
    raise SystemExit(
        "expected alert event\n" + json.dumps(payloads[-1], indent=2)
    )
if alert.get("phase") != "API_HANG":
    raise SystemExit(f"expected API_HANG phase\n{json.dumps(alert, indent=2)}")
if alert.get("action") != expected_action:
    raise SystemExit(f"expected {expected_action} action\n{json.dumps(alert, indent=2)}")
checkpoint = alert.get("checkpoint")
if not checkpoint:
    raise SystemExit(f"expected checkpoint path\n{json.dumps(alert, indent=2)}")
expected_root = os.path.join(project_dir, ".saw", "checkpoints") + os.sep
if not checkpoint.startswith(expected_root):
    raise SystemExit(
        f"expected checkpoint under {expected_root!r}, got {checkpoint!r}\n{json.dumps(alert, indent=2)}"
    )
print(checkpoint)
PY
}

assert_checkpoint_artifacts() {
  local checkpoint_dir=$1
  local jsonl_file=$2
  local file_one=$3
  local file_two=$4

  python3 - "$checkpoint_dir" "$jsonl_file" "$file_one" "$file_two" <<'PY'
import json
import os
import re
import sys
from datetime import datetime, timezone

checkpoint_dir, jsonl_file, file_one, file_two = sys.argv[1:]
expected_files = [
    ("src/main.rs", file_one),
    ("notes/todo.txt", file_two),
]

state_path = os.path.join(checkpoint_dir, "saw-state.json")
manifest_path = os.path.join(checkpoint_dir, "manifest.json")
snapshot_path = os.path.join(checkpoint_dir, "session-snapshot.jsonl")

for path in (state_path, manifest_path, snapshot_path):
    if not os.path.exists(path):
        raise SystemExit(f"expected checkpoint artifact {path}")

for relative_path, source_path in expected_files:
    copied_path = os.path.join(checkpoint_dir, relative_path)
    if not os.path.exists(copied_path):
        raise SystemExit(f"expected copied file {copied_path}")
    with open(source_path, "rb") as src, open(copied_path, "rb") as dst:
        if src.read() != dst.read():
            raise SystemExit(f"copied file mismatch for {relative_path}")

with open(manifest_path, "r", encoding="utf-8") as fh:
    manifest = json.load(fh)
with open(state_path, "r", encoding="utf-8") as fh:
    state = json.load(fh)
with open(jsonl_file, "rb") as src, open(snapshot_path, "rb") as dst:
    if src.read() != dst.read():
        raise SystemExit("session JSONL snapshot did not match source")

expected_rel_paths = [relative_path for relative_path, _ in expected_files]
if manifest.get("files") != expected_rel_paths:
    raise SystemExit(
        f"expected manifest files {expected_rel_paths!r}, got {manifest.get('files')!r}"
    )
if state.get("session_jsonl_path") != jsonl_file:
    raise SystemExit(
        f"expected state.session_jsonl_path={jsonl_file!r}, got {state.get('session_jsonl_path')!r}"
    )
recent_files = [item.get("path") for item in state.get("recently_modified_files", [])]
expected_sources = [source_path for _, source_path in expected_files]
if recent_files != expected_sources:
    raise SystemExit(
        f"expected recently_modified_files {expected_sources!r}, got {recent_files!r}"
    )

basename = os.path.basename(checkpoint_dir)
if not re.fullmatch(r"\d{8}-\d{6}", basename):
    raise SystemExit(f"checkpoint directory {basename!r} did not match YYYYMMDD-HHMMSS")

dir_timestamp = datetime.strptime(basename, "%Y%m%d-%H%M%S").replace(tzinfo=timezone.utc)
created_at = manifest.get("created_at")
if not created_at:
    raise SystemExit("manifest missing created_at")
manifest_timestamp = datetime.fromisoformat(created_at.replace("Z", "+00:00"))
if abs((manifest_timestamp - dir_timestamp).total_seconds()) > 2:
    raise SystemExit(
        f"manifest created_at {created_at!r} did not match checkpoint directory timestamp {basename!r}"
    )
if abs((datetime.now(timezone.utc) - manifest_timestamp).total_seconds()) > 30:
    raise SystemExit(f"manifest created_at {created_at!r} was not recent")
PY
}

run_checkpoint_case() {
  local mode=$1
  local expected_action=$2
  local expect_process_alive=$3
  local suffix=$4
  local project_dir="$TMP_DIR/project-$suffix"
  local jsonl_file="$TMP_DIR/session-$suffix.jsonl"
  local file_one="$project_dir/src/main.rs"
  local file_two="$project_dir/notes/todo.txt"
  local output
  local checkpoint_dir

  mkdir -p "$(dirname "$file_one")" "$(dirname "$file_two")"
  printf 'fn checkpoint() {}\n' > "$file_one"
  printf 'checkpoint me\n' > "$file_two"

  write_checkpoint_fixture "$jsonl_file" "ses-$suffix" "$file_one" "$file_two"

  sleep 600 &
  CHECKPOINT_PID=$!
  kill -STOP "$CHECKPOINT_PID"

  output=$(run_watch_capture "$project_dir" "$CHECKPOINT_PID" "$jsonl_file" "$mode")
  checkpoint_dir=$(assert_checkpoint_alert "$output" "$project_dir" "$expected_action")
  assert_checkpoint_artifacts "$checkpoint_dir" "$jsonl_file" "$file_one" "$file_two"

  if [[ "$expect_process_alive" == "yes" ]]; then
    kill -0 "$CHECKPOINT_PID" 2>/dev/null || fail "expected stopped process to remain alive"
    kill -CONT "$CHECKPOINT_PID"
    kill "$CHECKPOINT_PID" 2>/dev/null || true
    wait "$CHECKPOINT_PID" 2>/dev/null || true
  else
    kill -0 "$CHECKPOINT_PID" 2>/dev/null || fail "expected stopped process to remain alive until continued"
    kill -CONT "$CHECKPOINT_PID"
    wait "$CHECKPOINT_PID" 2>/dev/null || true

    if kill -0 "$CHECKPOINT_PID" 2>/dev/null; then
      fail "expected checkpoint-kill action to terminate the stopped process after SIGCONT"
    fi
  fi

  CHECKPOINT_PID=""
}

build_binary
run_checkpoint_case "--on-stuck=checkpoint-kill" "checkpoint-kill" no checkpoint-kill
run_checkpoint_case "--on-stuck=warn --checkpoint" "warn" yes checkpoint-warn

echo "ok - checkpoint save created artifacts for checkpoint-kill and --checkpoint stuck alerts"
