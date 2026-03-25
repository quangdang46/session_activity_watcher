# MANUAL_TEST_SCENARIOS

These scenarios exercise the live stuck-state classifications against a real Claude Code session.
Run them in a disposable session: several scenarios intentionally stop or kill the Claude process.

## Shared setup

From the repo root:

```bash
cargo build --bin saw
mkdir -p tmp/manual
```

Use two terminals:

- **Terminal A:** run Claude Code in this repo.
- **Terminal B:** run `saw` and the verification commands below.

A streaming monitor is the easiest way to catch transient phases:

```bash
cargo run -- watch --dir "$PWD" --timeout-secs 120 --robot --no-color | tee /tmp/saw-watch.jsonl
```

Add `--checkpoint` when you want every stuck alert to save a recovery snapshot before the configured action runs.

Optional helper to export the active Claude PID, session id, and JSONL path for the current working directory:

```bash
eval "$(python3 - <<'PY'
import json
import os
from pathlib import Path

cwd = Path(os.getcwd()).resolve()
sessions_dir = Path.home() / '.claude' / 'sessions'
for session_file in sorted(sessions_dir.glob('*.json'), key=lambda p: p.stat().st_mtime, reverse=True):
    data = json.loads(session_file.read_text())
    if Path(data['cwd']).resolve() == cwd:
        slug = str(cwd).replace('/', '-')
        jsonl = Path.home() / '.claude' / 'projects' / slug / f"{data['sessionId']}.jsonl"
        print(f"export CLAUDE_PID={data['pid']}")
        print(f"export CLAUDE_SESSION_ID={data['sessionId']}")
        print(f"export CLAUDE_SESSION_FILE='{session_file}'")
        print(f"export CLAUDE_JSONL='{jsonl}'")
        break
else:
    raise SystemExit('no active Claude session found for this cwd')
PY
)"

echo "PID=$CLAUDE_PID SESSION=$CLAUDE_SESSION_ID JSONL=$CLAUDE_JSONL"
```

---

## 1. Real API hang

Goal: pause Claude while it is waiting on a tool result and verify that `saw` reports `API_HANG` after the timeout window.

### Steps

1. In Terminal A, ask Claude to run a long tool call, for example `sleep 600`.
2. As soon as the tool is running, export the live PID with the helper above.
3. In Terminal B, start the monitor if it is not already running:

   ```bash
   cargo run -- watch --dir "$PWD" --timeout-secs 120 --robot --no-color | tee /tmp/saw-api-hang.jsonl
   ```

4. Pause Claude mid-tool-call:

   ```bash
   kill -STOP "$CLAUDE_PID"
   ```

5. Wait at least 121 seconds.
6. After verification, resume the process:

   ```bash
   kill -CONT "$CLAUDE_PID"
   ```

### Expected behavior

- `saw watch` emits an alert with `phase="API_HANG"`.
- Human mode prints a line like `ALERT API_HANG after 120s ...`.
- One-shot status reports `API_HANG` and exits with code `1`.

### Verification commands

```bash
grep 'API_HANG' /tmp/saw-api-hang.jsonl
cargo run --quiet -- status --json --dir "$PWD" > /tmp/saw-api-hang-status.json; status=$?; cat /tmp/saw-api-hang-status.json; echo "exit=$status"
python3 - <<'PY'
import json
from pathlib import Path
payload = json.loads(Path('/tmp/saw-api-hang-status.json').read_text())
assert payload['phase'] == 'API_HANG', payload
PY
```

---

## 2. Real tool loop

Goal: repeatedly rewrite the same file and verify that `saw` reports `TOOL_LOOP` once it sees 3 rewrites inside the 5-minute loop window.

### Steps

1. In Terminal B, start the monitor:

   ```bash
   cargo run -- watch --dir "$PWD" --timeout-secs 120 --robot --no-color | tee /tmp/saw-tool-loop.jsonl
   ```

2. Start a tight rewrite loop in another shell:

   ```bash
   cat > /tmp/rewrite-loop.sh <<'EOF'
   #!/usr/bin/env bash
   set -euo pipefail
   file=$1
   mkdir -p "$(dirname "$file")"
   while true; do
     date --iso-8601=seconds > "$file"
     sleep 1
   done
   EOF
   chmod +x /tmp/rewrite-loop.sh
   /tmp/rewrite-loop.sh "$PWD/tmp/manual/loop.txt" &
   LOOP_PID=$!
   echo "$LOOP_PID"
   ```

3. Let the script rewrite the file at least 3 times.
4. Stop the loop after verification:

   ```bash
   kill "$LOOP_PID"
   wait "$LOOP_PID" 2>/dev/null || true
   ```

### Expected behavior

- In a watcher-enabled live path, `saw` transitions to `TOOL_LOOP` and reports the rewritten file plus a count of at least `3`.
- `status --json` should eventually report `phase="TOOL_LOOP"` with the loop file in the details/state.

### Verification commands

```bash
grep 'TOOL_LOOP' /tmp/saw-tool-loop.jsonl
cargo run --quiet -- status --json --dir "$PWD" > /tmp/saw-tool-loop-status.json; status=$?; cat /tmp/saw-tool-loop-status.json; echo "exit=$status"
```

### Current limitation

The plain CLI watch path currently tails session JSONL and process metrics only. A standalone external writer loop will not trip `TOOL_LOOP` there until live file-watcher events are wired into the command path. If you need a CLI-only smoke test today, use repeated Claude `Write`/`Edit` calls against the same file instead of the external script.

---

## 3. Real scope leak

Goal: keep `saw` guarded to one subtree, then make Claude write outside that guard and verify that `saw` reports `SCOPE_LEAKING`.

### Steps

1. Prepare a guarded tree:

   ```bash
   mkdir -p src/auth src/billing
   ```

2. In Terminal B, start a guarded monitor:

   ```bash
   cargo run -- watch --dir "$PWD" --guard src/auth --robot --no-color | tee /tmp/saw-scope-leak.jsonl
   ```

3. In Terminal A, ask Claude to create or edit a file outside the guard, for example `src/billing/leak.rs`.
4. Leave the monitor running until the alert is emitted.

### Expected behavior

- `saw watch` emits an alert with `phase="SCOPE_LEAKING"`.
- The alert message names both the violating file and the configured guard.
- `status --json --guard src/auth` exits with code `3`.

### Verification commands

```bash
grep 'SCOPE_LEAKING' /tmp/saw-scope-leak.jsonl
cargo run --quiet -- status --json --dir "$PWD" --guard src/auth > /tmp/saw-scope-leak-status.json; status=$?; cat /tmp/saw-scope-leak-status.json; echo "exit=$status"
python3 - <<'PY'
import json
from pathlib import Path
payload = json.loads(Path('/tmp/saw-scope-leak-status.json').read_text())
assert payload['phase'] == 'SCOPE_LEAKING', payload
assert 'src/auth' in ' '.join(payload.get('guard_paths', [])), payload
PY
```

---

## 4. Real context reset

Goal: run a long enough Claude session to trigger compaction and verify that `saw` reports `CONTEXT_RESET` when the `compact_boundary` record appears.

### Steps

1. In Terminal B, start a robot monitor:

   ```bash
   cargo run -- watch --dir "$PWD" --timeout-secs 120 --robot --no-color | tee /tmp/saw-context-reset.jsonl
   ```

2. In Terminal A, keep the session running until Claude compacts. The easiest way is a long conversation with enough file reads/edits to force context compaction.
3. Watch the stream for a `CONTEXT_RESET` phase change.
4. After the compact boundary lands, send one more normal prompt so the session can move back to `WORKING` or `THINKING`.

### Expected behavior

- `saw watch --robot` emits a record with `phase="CONTEXT_RESET"`.
- The underlying Claude session JSONL contains a `compact_boundary` system record.
- `CONTEXT_RESET` is transient: after the next non-compact event, the phase should leave `CONTEXT_RESET`.

### Verification commands

```bash
grep 'CONTEXT_RESET' /tmp/saw-context-reset.jsonl
grep 'compact_boundary' "$CLAUDE_JSONL"
```

---

## 5. Process kill

Goal: kill Claude mid-session and verify that `saw` reports `DEAD`.

### Steps

1. Export the live PID with the helper above.
2. In Terminal B, start the monitor:

   ```bash
   cargo run -- watch --dir "$PWD" --timeout-secs 120 --robot --no-color | tee /tmp/saw-dead.jsonl
   ```

3. Kill Claude mid-session:

   ```bash
   kill -9 "$CLAUDE_PID"
   ```

4. Wait for `saw` to emit the final alert and exit.

### Expected behavior

- `saw watch` emits an alert with `phase="DEAD"`.
- The alert message says the process exited and Claude is no longer alive.
- `status --json` reports `phase="DEAD"`, `process_alive=false`, and exits with code `1`.

### Verification commands

```bash
grep 'DEAD' /tmp/saw-dead.jsonl || true
cargo run --quiet -- status --json --dir "$PWD" > /tmp/saw-dead-status.json; status=$?; cat /tmp/saw-dead-status.json; echo "exit=$status"
python3 - <<'PY'
import json
from pathlib import Path
payload = json.loads(Path('/tmp/saw-dead-status.json').read_text())
assert payload['phase'] == 'DEAD', payload
assert payload['process_alive'] is False, payload
PY
ps -p "$CLAUDE_PID" || true
```
