#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
BIN="$ROOT_DIR/target/debug/saw"
TMP_DIR=$(mktemp -d)
WARN_PID=""
BELL_PID=""
KILL_PID=""
CHECKPOINT_PID=""
ALIVE_PID=""

cleanup() {
  set +e

  if [[ -n "$ALIVE_PID" ]] && kill -0 "$ALIVE_PID" 2>/dev/null; then
    kill "$ALIVE_PID" 2>/dev/null
  fi

  if [[ -n "$WARN_PID" ]] && kill -0 "$WARN_PID" 2>/dev/null; then
    kill -CONT "$WARN_PID" 2>/dev/null
    kill "$WARN_PID" 2>/dev/null
  fi

  if [[ -n "$BELL_PID" ]] && kill -0 "$BELL_PID" 2>/dev/null; then
    kill -CONT "$BELL_PID" 2>/dev/null
    kill "$BELL_PID" 2>/dev/null
  fi

  if [[ -n "$KILL_PID" ]] && kill -0 "$KILL_PID" 2>/dev/null; then
    kill -CONT "$KILL_PID" 2>/dev/null
    kill "$KILL_PID" 2>/dev/null
  fi

  if [[ -n "$CHECKPOINT_PID" ]] && kill -0 "$CHECKPOINT_PID" 2>/dev/null; then
    kill -CONT "$CHECKPOINT_PID" 2>/dev/null
    kill "$CHECKPOINT_PID" 2>/dev/null
  fi

  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

build_binary() {
  cargo build --quiet --manifest-path "$ROOT_DIR/Cargo.toml" --bin saw
}

path_to_slug() {
  printf '%s' "$1" | tr '/' '-'
}

iso_seconds_ago() {
  python3 - "$1" <<'PY'
from datetime import datetime, timedelta, timezone
import sys

seconds = int(sys.argv[1])
value = datetime.now(timezone.utc) - timedelta(seconds=seconds)
print(value.replace(microsecond=0).isoformat().replace("+00:00", "Z"))
PY
}

assert_api_hang_output() {
  local output=$1
  local case_name=$2

  if [[ ! "$output" =~ ALERT\ API_HANG\ after\ ([0-9]+)s ]]; then
    printf 'expected ApiHang alert for %s\n%s\n' "$case_name" "$output" >&2
    return 1
  fi

  if (( BASH_REMATCH[1] < 120 )); then
    printf 'expected ApiHang detection at or after 120s for %s, got %ss\n%s\n' "$case_name" "${BASH_REMATCH[1]}" "$output" >&2
    return 1
  fi
}

run_watch_capture() {
  local home_dir=$1
  shift
  local output
  local status

  set +e
  output=$(HOME="$home_dir" timeout 5 "$BIN" watch "$@" 2>&1)
  status=$?
  set -e

  if [[ $status -ne 0 && $status -ne 124 ]]; then
    printf 'watch command failed with status %s\n%s\n' "$status" "$output" >&2
    return 1
  fi

  printf '%s' "$output"
}

write_session_fixture() {
  local home_dir=$1
  local project_dir=$2
  local session_id=$3
  local pid=$4
  local tool_timestamp=$5
  local started_at=$6
  local session_file="$home_dir/.claude/sessions/${session_id}.json"
  local slug
  slug=$(path_to_slug "$project_dir")
  local jsonl_file="$home_dir/.claude/projects/$slug/${session_id}.jsonl"

  mkdir -p "$(dirname "$session_file")" "$(dirname "$jsonl_file")"

  python3 - "$session_file" "$jsonl_file" "$project_dir" "$session_id" "$pid" "$tool_timestamp" "$started_at" <<'PY'
import json
import sys

session_file, jsonl_file, project_dir, session_id, pid, tool_timestamp, started_at = sys.argv[1:]

with open(session_file, "w", encoding="utf-8") as fh:
    json.dump(
        {
            "pid": int(pid),
            "sessionId": session_id,
            "cwd": project_dir,
            "startedAt": int(started_at),
        },
        fh,
    )

records = [
    {
        "type": "session_started",
        "timestamp": tool_timestamp,
        "sessionId": session_id,
    },
    {
        "type": "assistant",
        "timestamp": tool_timestamp,
        "message": {
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 1},
            "content": [
                {
                    "type": "tool_use",
                    "name": "Bash",
                    "input": {"command": "sleep 600"},
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

run_warn_case() {
  local home_dir="$TMP_DIR/warn-home"
  local project_dir="$TMP_DIR/warn-project"
  local output

  mkdir -p "$project_dir"
  sleep 600 &
  WARN_PID=$!
  kill -STOP "$WARN_PID"

  write_session_fixture \
    "$home_dir" \
    "$project_dir" \
    "ses-api-hang-warn" \
    "$WARN_PID" \
    "$(iso_seconds_ago 121)" \
    "$(date +%s)"

  output=$(run_watch_capture "$home_dir" --dir "$project_dir" --timeout-secs 120 --on-stuck warn --no-color)

  assert_api_hang_output "$output" "warn case"

  kill -0 "$WARN_PID" 2>/dev/null || {
    printf 'expected warned process to remain alive\n' >&2
    return 1
  }

  kill -CONT "$WARN_PID"
  kill "$WARN_PID"
  wait "$WARN_PID" 2>/dev/null || true
  WARN_PID=""
}

run_bell_case() {
  local home_dir="$TMP_DIR/bell-home"
  local project_dir="$TMP_DIR/bell-project"
  local output

  mkdir -p "$project_dir"
  sleep 600 &
  BELL_PID=$!
  kill -STOP "$BELL_PID"

  write_session_fixture \
    "$home_dir" \
    "$project_dir" \
    "ses-api-hang-bell" \
    "$BELL_PID" \
    "$(iso_seconds_ago 121)" \
    "$(date +%s)"

  output=$(run_watch_capture "$home_dir" --dir "$project_dir" --timeout-secs 120 --on-stuck bell --no-color)

  assert_api_hang_output "$output" "bell case"
  [[ "$output" == *$'\a'* ]] || {
    printf 'expected bell output to include terminal bell\n%s\n' "$output" >&2
    return 1
  }
  [[ "$output" == *"action=bell"* ]] || {
    printf 'expected bell output to report bell action\n%s\n' "$output" >&2
    return 1
  }

  kill -0 "$BELL_PID" 2>/dev/null || {
    printf 'expected bell process to remain alive\n' >&2
    return 1
  }

  kill -CONT "$BELL_PID"
  kill "$BELL_PID"
  wait "$BELL_PID" 2>/dev/null || true
  BELL_PID=""
}

run_kill_case() {
  local home_dir="$TMP_DIR/kill-home"
  local project_dir="$TMP_DIR/kill-project"
  local output

  mkdir -p "$project_dir"
  sleep 600 &
  KILL_PID=$!
  kill -STOP "$KILL_PID"

  write_session_fixture \
    "$home_dir" \
    "$project_dir" \
    "ses-api-hang-kill" \
    "$KILL_PID" \
    "$(iso_seconds_ago 121)" \
    "$(date +%s)"

  output=$(run_watch_capture "$home_dir" --dir "$project_dir" --timeout-secs 120 --on-stuck kill --no-color)

  assert_api_hang_output "$output" "kill case"

  kill -0 "$KILL_PID" 2>/dev/null || {
    printf 'expected stopped process to remain alive until continued\n' >&2
    return 1
  }

  kill -CONT "$KILL_PID"
  wait "$KILL_PID" 2>/dev/null || true

  if kill -0 "$KILL_PID" 2>/dev/null; then
    printf 'expected kill action to terminate the stopped process after SIGCONT\n' >&2
    return 1
  fi

  KILL_PID=""
}

run_checkpoint_kill_case() {
  local home_dir="$TMP_DIR/checkpoint-home"
  local project_dir="$TMP_DIR/checkpoint-project"
  local output
  local checkpoint_root

  mkdir -p "$project_dir"
  printf 'fn main() {}\n' > "$project_dir/main.rs"
  sleep 600 &
  CHECKPOINT_PID=$!
  kill -STOP "$CHECKPOINT_PID"

  write_session_fixture \
    "$home_dir" \
    "$project_dir" \
    "ses-api-hang-checkpoint" \
    "$CHECKPOINT_PID" \
    "$(iso_seconds_ago 121)" \
    "$(date +%s)"

  output=$(run_watch_capture "$home_dir" --dir "$project_dir" --timeout-secs 120 --on-stuck checkpoint-kill --no-color)

  assert_api_hang_output "$output" "checkpoint-kill case"
  [[ "$output" == *"action=checkpoint-kill"* ]] || {
    printf 'expected checkpoint-kill action in output\n%s\n' "$output" >&2
    return 1
  }

  checkpoint_root="$project_dir/.saw/checkpoints"
  [[ -d "$checkpoint_root" ]] || {
    printf 'expected checkpoint directory to be created\n%s\n' "$output" >&2
    return 1
  }
  find "$checkpoint_root" -mindepth 1 -maxdepth 1 -type d | grep -q . || {
    printf 'expected checkpoint snapshot directory\n%s\n' "$output" >&2
    return 1
  }

  kill -0 "$CHECKPOINT_PID" 2>/dev/null || {
    printf 'expected stopped process to remain alive until continued after checkpoint-kill\n' >&2
    return 1
  }

  kill -CONT "$CHECKPOINT_PID"
  wait "$CHECKPOINT_PID" 2>/dev/null || true

  if kill -0 "$CHECKPOINT_PID" 2>/dev/null; then
    printf 'expected checkpoint-kill action to terminate the stopped process after SIGCONT\n' >&2
    return 1
  fi

  CHECKPOINT_PID=""
}

run_alive_case() {
  local home_dir="$TMP_DIR/alive-home"
  local project_dir="$TMP_DIR/alive-project"
  local output

  mkdir -p "$project_dir"
  sleep 600 &
  ALIVE_PID=$!

  write_session_fixture \
    "$home_dir" \
    "$project_dir" \
    "ses-api-hang-alive" \
    "$ALIVE_PID" \
    "$(iso_seconds_ago 1)" \
    "$(date +%s)"

  output=$(run_watch_capture "$home_dir" --dir "$project_dir" --timeout-secs 120 --on-stuck kill --no-color)

  [[ "$output" != *"ALERT"* ]] || {
    printf 'did not expect an alert for the live process\n%s\n' "$output" >&2
    return 1
  }

  kill -0 "$ALIVE_PID" 2>/dev/null || {
    printf 'expected live process to remain alive when no ApiHang is detected\n' >&2
    return 1
  }
}

build_binary
run_warn_case
run_bell_case
run_kill_case
run_checkpoint_kill_case
run_alive_case

echo "ok - ApiHang detected after 120s timeout, warn/bell/kill/checkpoint-kill actions behaved correctly, and live process was not interrupted"
