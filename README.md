# saw

> Watch Claude Code session activity and catch stuck states before they waste time.

`saw` is a Rust CLI that watches Claude Code session JSONL, hook events, and process metrics so you can tell whether an agent is working, thinking, hung, looping, compacted, dead, or writing outside the subtree you meant it to touch.

## Demo

![saw demo](assets/saw-demo.gif)

The demo shows `saw watch` catching a stuck Claude session and surfacing the alert live instead of leaving you guessing at a spinner.

## Why use saw?

- Catch API hangs after a configurable silence window.
- Spot tool loops and repeated failing test loops.
- Guard a subtree and alert on out-of-scope writes.
- See context compactions as `CONTEXT_RESET`.
- Get a one-shot status command with useful exit codes.
- Use a TUI when you want a dashboard, or `--robot` JSON when you want automation.
- Save a recovery checkpoint before killing a stuck run.

## Install

### Cargo

#### Install from GitHub

```bash
cargo install --git https://github.com/quangdang46/session_activity_watcher --locked --package saw
```

#### Install from a local checkout

```bash
git clone https://github.com/quangdang46/session_activity_watcher.git
cd session_activity_watcher
cargo install --path crates/saw-cli --locked
```

### curl | bash

```bash
curl -fsSL "https://raw.githubusercontent.com/quangdang46/session_activity_watcher/main/install.sh?$(date +%s)" | bash
```

The installer picks the matching release asset for Linux x86_64/aarch64, macOS x86_64/aarch64, or Windows x86_64. If a prebuilt archive is unavailable, it falls back to a source build.

Useful variants:

```bash
curl -fsSL "https://raw.githubusercontent.com/quangdang46/session_activity_watcher/main/install.sh?$(date +%s)" | bash -s -- --verify
```

Pass installer flags after `bash -s --`, for example `--system`, `--version v0.1.0`, `--from-source`, `--dest "$HOME/bin" --easy-mode`, or `--uninstall`.

By default the installer writes to `~/.local/bin`. Use `--system` for `/usr/local/bin`, `--dest` for a custom location, or `--easy-mode` to append the install dir to `~/.bashrc` and `~/.zshrc`.

### Build manually

```bash
cargo build --release -p saw --bin saw
./target/release/saw --help
```

## Quick start

Most commands auto-detect the newest live Claude Code session for the directory you pass with `--dir`. Use `--session` or `--pid` when you want to pin a specific session.

### Watch the current repo

```bash
saw watch --dir "$PWD"
```

### Emit machine-readable JSON

```bash
saw watch --dir "$PWD" --robot --no-color
```

### Guard a subtree and bell on scope leaks

```bash
saw watch --dir "$PWD" --guard src/auth --on-scope-leak bell
```

### Save a checkpoint before killing a stuck run

```bash
saw watch --dir "$PWD" --on-stuck checkpoint-kill
```

### Inspect the current session once

```bash
saw status --dir "$PWD" --json
```

### Open the dashboard

```bash
saw tui --dir "$PWD"
```

## Features with examples

### API hang detection

```bash
saw watch --dir "$PWD" --timeout 2m --on-stuck bell
```

Alerts when Claude appears stuck waiting on the API instead of making progress.

### Tool and test loop detection

```bash
saw status --dir "$PWD" --json
```

Reports `TOOL_LOOP` or `TEST_LOOP` when recent Claude activity shows repeated rewrites or repeated failing test churn.

### Scope guardrails

```bash
saw watch --dir "$PWD" --guard src/auth,tests/auth --on-scope-leak kill
```

Flags `SCOPE_LEAKING` as soon as the session writes outside the allowed subtree.

### Context reset visibility

```bash
saw watch --dir "$PWD" --robot --no-color
```

Emits `CONTEXT_RESET` when Claude compacts and crosses a compact boundary.

### Dead-session detection

```bash
saw status --dir "$PWD"
```

Treats a vanished or stalled Claude process as `DEAD` instead of leaving you to infer it manually.

### Checkpoints

```bash
saw watch --dir "$PWD" --checkpoint --on-stuck checkpoint-kill
```

Copies recent modified files, the current state snapshot, and the session JSONL into `.saw/checkpoints/<timestamp>/`.

### Dashboard and JSON automation

```bash
saw tui --dir "$PWD"
saw watch --dir "$PWD" --robot --no-color
```

Use the TUI when you want live triage and `--robot` when you want to feed another tool.

## Command reference

| Command | What it does | Example |
| --- | --- | --- |
| `saw watch` | Continuously monitors a live Claude Code session and emits status plus alerts. | `saw watch --dir "$PWD" --timeout 2m --guard src/auth` |
| `saw status` | Prints one snapshot and exits with a meaningful code. | `saw status --dir "$PWD" --json` |
| `saw tui` | Opens the ratatui dashboard with status, metrics, guard state, and recent alerts. | `saw tui --dir "$PWD" --guard src/auth` |
| `saw config` | Reads or updates persistent defaults for timeout and stuck action. | `saw config --timeout 2m --on-stuck bell` |
| `saw hook` | Normalizes Claude hook payloads into `.saw/hooks/<session>.jsonl`. | `printf '{"hook_event_name":"SessionStart","session_id":"ses-demo"}' \| saw hook --session-start --dir "$PWD"` |

### `saw watch`

Live monitoring for the current repo or a pinned Claude session.

Useful flags:

- `--pid <PID>`: watch a specific Claude process.
- `--session <SESSION>`: watch a specific Claude session id.
- `--dir <DIR>`: project root used for auto-detection.
- `--timeout <DURATION>`: stuck threshold, for example `120s` or `2m`.
- `--guard <PATH[,PATH...]>`: allowed write subtrees.
- `--on-stuck <warn|bell|kill|checkpoint-kill>`: action for API hangs.
- `--checkpoint`: save a checkpoint before the configured stuck action runs.
- `--on-scope-leak <warn|bell|kill>`: action for out-of-scope writes.
- `--robot`: emit JSON records instead of human-readable status lines.
- `--quiet`: suppress non-essential alert output.
- `--no-color`: disable ANSI color in human mode.
- `--force-poll`: use polling instead of filesystem notifications.

### `saw status`

One-shot status for shell scripts, CI, or a quick manual check.

Useful flags:

- `--file <FILE>`: inspect a specific session JSONL file.
- `--dir <DIR>`: project root used for session lookup.
- `--timeout <DURATION>`: stuck threshold override.
- `--session <SESSION>`: inspect one session id.
- `--all`: print all matching live sessions for the directory.
- `--json`: print full JSON instead of a compact line.
- `--guard <PATH[,PATH...]>`: apply guard paths for scope-leak classification.

Exit codes:

| Code | Meaning |
| --- | --- |
| `0` | `WORKING`, `THINKING`, or another healthy state |
| `1` | `API_HANG` or `DEAD` |
| `2` | `TOOL_LOOP` or `TEST_LOOP` |
| `3` | `SCOPE_LEAKING` |
| `4` | `IDLE` |

### `saw tui`

Interactive dashboard with live status, guard state, metrics, file activity, and alerts.

Useful flags:

- `--file <FILE>`: inspect a specific session JSONL file.
- `--dir <DIR>`: project root used for session lookup.
- `--timeout <DURATION>`: stuck threshold override.
- `--refresh <REFRESH>`: dashboard refresh interval in milliseconds.
- `--guard <PATH[,PATH...]>`: apply guard paths in the dashboard.

Keyboard shortcuts:

| Key | Action |
| --- | --- |
| `q` | Quit |
| `k` | Interrupt Claude |
| `K` | Force kill Claude |
| `c` | Save a checkpoint |
| `g` | Edit guard paths |
| `Tab` | Switch active scroll panel |
| `↑` / `↓` | Scroll the active panel |
| `PageUp` / `PageDown` | Scroll faster |
| `Home` / `End` | Jump to top or bottom |

### `saw config`

Persist default timeout and stuck-action preferences in `~/.config/saw/config.toml`.

Useful flags:

- `--list`: print the current config.
- `--timeout <DURATION>`: set the default timeout.
- `--on-stuck <warn|bell|kill|checkpoint-kill>`: set the default stuck action.
- `--reset`: restore defaults.

### `saw hook`

Accepts Claude hook payloads on stdin and writes normalized events into `.saw/hooks/<session>.jsonl`.

Useful flags:

- `--pre`: treat stdin as a `PreToolUse` payload.
- `--session-start`: treat stdin as a `SessionStart` payload.
- `--dir <DIR>`: project root where `.saw/hooks/` should be written.

Examples:

```bash
printf '{"hook_event_name":"SessionStart","session_id":"ses-demo"}' | saw hook --session-start --dir "$PWD"
printf '{"hook_event_name":"PreToolUse","session_id":"ses-demo","tool_name":"Write","tool_input":{"file_path":"src/lib.rs"}}' | saw hook --pre --dir "$PWD"
```

## Configuration

### Config file

`saw config` stores persistent defaults at `~/.config/saw/config.toml`.

Example:

```toml
timeout = "130s"
on_stuck = "warn"
```

Update it from the CLI:

```bash
saw config --timeout 2m --on-stuck bell
saw config --list
saw config --reset
```

### Runtime files

`saw` also writes a small amount of runtime state inside the repo you are watching:

- `.saw/hooks/<session>.jsonl`: normalized Claude hook events from `saw hook`.
- `.saw/checkpoints/<timestamp>/`: saved checkpoints created by `--checkpoint` or the TUI.

Guard paths are per-run settings today, so set them on the command line:

```bash
saw watch --dir "$PWD" --guard src/auth
```

## Contributing

Issues and pull requests are welcome.

Before opening a PR, run the usual checks from the repo root:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
bash tests/e2e/test_api_hang.sh
bash tests/e2e/test_scope_leak.sh
```

For live end-to-end validation against a real Claude session, use the scenarios in [`tests/MANUAL_TEST_SCENARIOS.md`](tests/MANUAL_TEST_SCENARIOS.md).

## License

Intended license: MIT. Add a top-level `LICENSE` file before publishing a public release so the repository contents match the README.
