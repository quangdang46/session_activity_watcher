# saw — Session Activity Watcher
> "You shouldn't have to stare at a blinking cursor wondering if your agent is working or dead."

---

## Table of Contents

1. [Problem Statement](#1-problem-statement)
2. [Why This Exists — Pain Point Deep Dive](#2-why-this-exists--pain-point-deep-dive)
3. [Prior Art & Competitive Landscape](#3-prior-art--competitive-landscape)
4. [Solution Design](#4-solution-design)
5. [Technical Research](#5-technical-research)
6. [Architecture](#6-architecture)
7. [Data Sources](#7-data-sources)
8. [Core Algorithms](#8-core-algorithms)
9. [Crate Structure](#9-crate-structure)
10. [CLI Surface](#10-cli-surface)
11. [TUI Design](#11-tui-design)
12. [Implementation Plan — Phase by Phase](#12-implementation-plan--phase-by-phase)
13. [Testing Strategy](#13-testing-strategy)
14. [Release & Distribution](#14-release--distribution)
15. [Success Metrics](#15-success-metrics)
16. [Open Questions](#16-open-questions)

---

## 1. Problem Statement

When Claude Code (or any AI coding agent) runs autonomously, it becomes a **black box**.
The user sees a spinner and a message like:

```
⠙ Photosynthesizing… 8m 35s
```

There is no way to know from the outside whether the agent is:

- Actively thinking and about to respond
- Waiting on a slow network response (SSE timeout)
- Caught in a tool-call loop, rewriting the same file repeatedly
- Completely dead — process alive, doing nothing
- Making progress but in the wrong direction (scope drift)

This forces users into a guessing game: **wait more** or **kill it**. Both are lossy:
- Waiting on a dead agent wastes minutes or hours
- Killing an active agent loses progress and requires expensive context reconstruction

`saw` solves this by observing the agent **from the outside** using OS-level signals and
Claude Code's own session JSONL files — no patching, no API keys, no modifications to the
agent itself required.

---

## 2. Why This Exists — Pain Point Deep Dive

### 2.1 The Silence Problem

Claude Code communicates progress through:
1. Terminal spinner text
2. Occasional tool-use output printed to stdout

Neither gives you signal about **internal state**. The spinner keeps spinning whether the
agent is actively processing or waiting for a packet that will never arrive.

Real user reports (GitHub issues, Reddit, HN threads):
- "It's been thinking for 10 minutes, is this normal?"
- "No activity for 8m 35s — should I interrupt?"
- "I killed it after 15 minutes, turned out it was almost done"
- "Claude code just sat there for 45 minutes burning tokens on a loop"

### 2.2 Three Distinct Stuck Types — Users Can't Tell Apart

#### Type A — API Hang (SSE Timeout)

**What happens:** Claude Code maintains an SSE (Server-Sent Events) connection to
Anthropic's API. Occasionally this connection drops silently. The process is alive, token
counter is frozen, no file activity.

**Detection signals:**
- `CPU usage → 0%`
- `token count frozen for > 120 seconds`
- `no file system events for > 120 seconds`
- `network socket in CLOSE_WAIT or TIME_WAIT state`

**What users should do:** Send a follow-up message (kicks SSE reconnect ~60% of cases),
or Ctrl+C and resume.

**Why it's hard to detect manually:** Process appears alive. Spinner still spins. No error
message. Only way to know is to watch token counter closely for 2+ minutes.

#### Type B — Tool Loop

**What happens:** Agent enters a loop — typically `Read → Write → Read → Write` on the
same file, often because a test keeps failing or the agent misunderstands a constraint.
Token counter keeps rising. File keeps being modified. No net progress.

**Detection signals:**
- `same file written > 3 times in 5 minutes`
- `same tool called > 5 times consecutively without state change`
- `test runner invoked > N times, all failures`

**What users should do:** Interrupt, inspect, add clarification to the task.

**Why it's hard to detect manually:** The agent looks "active" — tokens rising, file events
happening. Users assume it's working. Loop can run for 20+ minutes before user notices.

#### Type C — Dead UI

**What happens:** Claude Code UI is unresponsive. The prompt field is highlighted but
no thinking indicator, no action. Submitting a new prompt replaces the current one but
nothing happens.

**Detection signals:**
- `Claude Code process CPU = 0%`
- `token count frozen`
- `no JSONL records written for > 5 minutes`
- `process memory flat (no allocation activity)`

**What users should do:** Hard kill and restart session.

### 2.3 The Scope Drift Problem

Even when the agent is working correctly (not stuck), it may be working on the **wrong
thing**. An agent asked to fix authentication might:
- Decide to refactor the logging module "while it's in there"
- Touch billing code because it found a related bug
- Rewrite a utility that was out of scope

Without real-time file monitoring, users discover this after the fact — when reviewing a
diff that's 10x larger than expected.

`saw --guard src/auth/` detects and alerts on out-of-scope file modifications in
real-time, before the agent has spent 20 minutes on the wrong thing.

### 2.4 The Context Reset Problem

When an agent hits its context window limit, Claude Code performs a "compact" operation —
summarizing the conversation. This:
- Loses nuance from earlier in the session
- Sometimes causes the agent to forget constraints set at the start
- Can cause the agent to repeat work it already did

This is visible in the JSONL as a `compact_boundary` system record. No existing tool
surfaces this to the user in real-time.

### 2.5 Why No Existing Tool Solves This

| Tool | What it does | Gap |
|------|-------------|-----|
| Claude Code built-in | Spinner + tool output | No external observability |
| `htop`/`btop` | System-wide CPU/mem | No agent-specific semantics |
| `process-compose` | Generic process supervisor | No agent-specific semantics |
| `watchexec` | File watcher, re-run on change | No agent semantics, no stuck detection |
| `watch` (Unix) | Repeat command | No intelligence |

There is a complete gap in the **real-time, semantics-aware agent monitoring** space.

---

## 3. Prior Art & Competitive Landscape

### 3.1 Existing Solutions Examined

**`process-compose`**
- Generic process supervisor
- No understanding of AI agent internals
- No file event monitoring
- No JSONL parsing

**`watchexec`**
- File watcher, re-runs commands on change
- No agent semantics, no stuck detection

**`cargo-watch`**
- Rust-specific file watcher
- Same limitations as watchexec

**CodeScene / Hotspots**
- Historical code analysis
- Post-hoc, not real-time
- No agent observability

### 3.2 What saw Does Differently

`saw` is purpose-built for one thing: **answering "what is my AI agent doing right now"
with enough precision to take action**.

The key insight is combining three independent signal sources:
1. OS process metrics (CPU, memory, IO) — always available
2. File system events (inotify/FSEvents/kqueue) — always available  
3. Agent's own JSONL session log — available when using Claude Code

Any two of these can be missing and saw still provides useful signal.
All three together give high-confidence stuck detection.

---

## 4. Solution Design

### 4.1 Design Principles

**Principle 1: Zero modification to the agent**
`saw` observes from outside. No patches to Claude Code, no API keys for Anthropic,
no changes to the agent's behavior. This means it works with any future version of
Claude Code, and works with other agents that write JSONL logs.

**Principle 2: Layered signals, graceful degradation**
If JSONL is not available → fall back to file events + process metrics.
If process metrics are unavailable → fall back to file events + JSONL.
At least one signal is always available.

**Principle 3: Actionable output**
Every alert includes a recommended action, not just a description of the problem.
"API hang detected" is not enough. "API hang detected → send follow-up message or Ctrl+C"
is actionable.

**Principle 4: Machine-readable by default**
`--robot` flag produces JSON stream suitable for consumption by any
orchestration tool or shell script. This makes `saw` a building block, not just a user-facing tool.

**Principle 5: Single binary, zero runtime deps**
`cargo install saw` is the entire installation. No Python, no Node, no systemd unit.
Works in CI, in Docker, in minimal environments.

### 4.2 What saw is NOT

- Not a process manager (use process-compose or similar for that)
- Not a cost tracker (out of scope)
- Not a code quality tool
- Not a replacement for reading Claude Code output
- Not an AI tool itself — no LLM calls, no embeddings, pure heuristics

---

## 5. Technical Research

### 5.1 Claude Code JSONL Format — Verified from Real Data

**⚠️ RESEARCH UPDATE (2026-03-21):** Schema below is verified against 55+ real session
files on the actual target machine. Previous assumptions were wrong in several places.

#### File location — SLUG not HASH

```
~/.claude/projects/<slug>/<session-uuid>.jsonl
```

**IMPORTANT:** The project folder name is NOT a hash. It is the absolute path
**slug-encoded** — forward slashes replaced with dashes:

```
/home/quangdang/projects/tools/linehash
→ -home-quangdang-projects-tools-linehash
```

Finding the right JSONL file for a given PID:

```rust
// Step 1: read sessions/<pid>.json → get cwd + sessionId
// Step 2: slug-encode cwd → find project folder
// Step 3: open <project-folder>/<sessionId>.jsonl

fn path_to_slug(path: &Path) -> String {
    path.to_str()
        .unwrap_or("")
        .replace('/', "-")
}

fn find_jsonl_for_pid(home: &Path, pid: u32) -> Option<PathBuf> {
    let session_path = home.join(format!(".claude/sessions/{}.json", pid));
    let raw = fs::read_to_string(&session_path).ok()?;
    let session: SessionFile = serde_json::from_str(&raw).ok()?;
    let slug = path_to_slug(Path::new(&session.cwd));
    let jsonl = home.join(format!(
        ".claude/projects/{}/{}.jsonl",
        slug,
        session.session_id
    ));
    Some(jsonl)
}
```

#### Top-level record types (all verified)

```
assistant              → agent message, contains tool_use inside content[]
user                   → user message OR tool_result (dual use!)
progress               → streaming progress indicator
file-history-snapshot  → file version snapshot bookmark
system                 → system events (compact_boundary etc.)
queue-operation        → internal queue state
last-prompt            → bookmark of last user prompt
custom-title           → session title update
agent-name             → agent display name update
```

**Critical:** `tool_result` is NOT a top-level type. It is nested inside a `user` record.

#### Nested content item types (inside `message.content[]`)

```
tool_use      → agent invoking a tool (inside assistant record)
tool_result   → tool output (inside user record)
thinking      → agent reasoning (inside assistant record)
text          → plain text (inside assistant or user record)
```

#### Full record shapes — verified

**`user` record (simple message):**
```json
{
  "type": "user",
  "uuid": "8a368bbf-df3f-487d-a6e9-b8048cc87247",
  "parentUuid": null,
  "isSidechain": false,
  "promptId": "28191b88-d928-4be9-82bb-b22646fbc152",
  "timestamp": "2026-03-21T09:48:02.645Z",
  "sessionId": "63235bb9-b558-421a-b5cd-4cfadd3d716a",
  "cwd": "/home/quangdang/.claude",
  "version": "2.1.81",
  "gitBranch": "HEAD",
  "permissionMode": "bypassPermissions",
  "userType": "external",
  "entrypoint": "cli",
  "message": {
    "role": "user",
    "content": "implement the login endpoint"
  }
}
```

**`assistant` record with `tool_use`:**
```json
{
  "type": "assistant",
  "uuid": "4ddaf440-5783-45ad-913a-12ead89e5fad",
  "parentUuid": "8a368bbf-...",
  "isSidechain": false,
  "timestamp": "2026-03-21T09:48:05.123Z",
  "sessionId": "63235bb9-...",
  "cwd": "/home/quangdang/.claude",
  "version": "2.1.81",
  "gitBranch": "HEAD",
  "slug": "woolly-forging-scott",
  "message": {
    "id": "resp_...",
    "type": "message",
    "role": "assistant",
    "model": "claude-opus-4-5",
    "stop_reason": "tool_use",
    "usage": { "input_tokens": 1234, "output_tokens": 56 },
    "content": [
      { "type": "thinking", "thinking": "I should write..." },
      {
        "type": "tool_use",
        "id": "call_lVDpNXyAadtRX8d29CxzqFqg",
        "name": "Write",
        "input": {
          "file_path": "src/auth/login.rs",
          "content": "..."
        }
      }
    ]
  }
}
```

**`user` record with `tool_result` (Bash):**
```json
{
  "type": "user",
  "sourceToolAssistantUUID": "4ddaf440-5783-45ad-913a-12ead89e5fad",
  "message": {
    "role": "user",
    "content": [
      {
        "type": "tool_result",
        "tool_use_id": "call_lVDpNXyAadtRX8d29CxzqFqg",
        "content": "total 1140\n...",
        "is_error": false
      }
    ]
  },
  "toolUseResult": {
    "stdout": "total 1140\n...",
    "stderr": "",
    "interrupted": false,
    "isImage": false,
    "noOutputExpected": false
  }
}
```

**`file-history-snapshot`:**
```json
{
  "type": "file-history-snapshot",
  "messageId": "8a368bbf-df3f-487d-a6e9-b8048cc87247",
  "isSnapshotUpdate": false,
  "snapshot": {
    "messageId": "8a368bbf-...",
    "trackedFileBackups": {},
    "timestamp": "2026-03-21T09:48:02.647Z"
  }
}
```

#### ⚠️ NO exit_code field — VERIFIED

Searched all `~/.claude/projects/**/*.jsonl` recursively. Result: **`exit_code` not found
anywhere.** Do not rely on exit code for TestLoop detection.

**What IS available for failure detection:**
- `tool_result.is_error: true` (boolean, sometimes present on Bash failures)
- `toolUseResult.stderr` (non-empty string = something went wrong)
- `toolUseResult.interrupted: true` (user or timeout interrupted)
- Content analysis: if stdout contains "FAILED", "error[E", "panicked" etc.

#### Large output handling — persisted-output pointer

When Bash output is too large, JSONL inline content is replaced with:
```
<persisted-output>
Output too large (...). Full output saved to:
/home/quangdang/.claude/projects/<slug>/<uuid>/tool-results/call_XYZ.txt

Preview (first 2KB):
...
</persisted-output>
```

Vigil must detect this pattern and optionally follow the pointer to the `.txt` file.

**Key fields for saw:**
- `timestamp` → calculate silence duration
- `type` → branch parsing logic
- `message.usage.input_tokens` + `output_tokens` → token activity (in `assistant` records)
- `message.content[].type == "tool_use"` + `.name` + `.input.file_path` → file activity
- `toolUseResult.stderr` + `.interrupted` → failure/interrupt signals (no exit_code!)
- `tool_result.is_error` → boolean failure flag
- `message.stop_reason == "tool_use"` → agent still has work to do
- `isSidechain: true` → subagent record, different weight in activity scoring

### 5.2 Sessions File — Verified Schema

**Location:** `~/.claude/sessions/<pid>.json`

**Verified across 55 real files — schema is stable and minimal:**

```rust
#[derive(Deserialize)]
pub struct SessionFile {
    pub pid: u32,
    #[serde(rename = "sessionId")]
    pub session_id: String,      // UUID → links to projects/<slug>/<sessionId>.jsonl
    pub cwd: String,             // absolute path → slug-encode to find project folder
    #[serde(rename = "startedAt")]
    pub started_at: u64,         // epoch milliseconds
}
```

**No optional fields observed.** All 4 fields present in every sample.

This is the **primary entry point** for saw's PID → session discovery:

```rust
fn discover_session(home: &Path, pid: u32) -> Result<SessionFile> {
    let path = home.join(format!(".claude/sessions/{}.json", pid));
    let content = fs::read_to_string(&path)
        .with_context(|| format!("No session file for PID {}", pid))?;
    serde_json::from_str(&content).context("Invalid session file format")
}
```

**Finding all active sessions:**
```rust
fn list_all_sessions(home: &Path) -> Vec<SessionFile> {
    let sessions_dir = home.join(".claude/sessions");
    fs::read_dir(&sessions_dir)
        .into_iter().flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension() == Some(OsStr::new("json")))
        .filter_map(|e| {
            let content = fs::read_to_string(e.path()).ok()?;
            serde_json::from_str(&content).ok()
        })
        .collect()
}

### 5.3 Claude Code Hooks — Config Schema

**Location:** `~/.claude/settings.json` (global) or `.claude/settings.json` (project-level)

**Verified:** `settings.json` on target machine already has `PreToolUse` hook for Bash.
`saw setup` injects additional PostToolUse + SessionStart hooks.

```json
{
  "hooks": {
    "PostToolUse": [{
      "matcher": "",
      "hooks": [{ "type": "command", "command": "saw hook" }]
    }],
    "PreToolUse": [{
      "matcher": "Write|Edit|MultiEdit|Bash",
      "hooks": [{ "type": "command", "command": "saw hook --pre" }]
    }],
    "SessionStart": [{
      "hooks": [{ "type": "command", "command": "saw hook --session-start" }]
    }]
  }
}
```

**Hook stdin payload:**
```json
{
  "hook_event_name": "PostToolUse",
  "session_id": "63235bb9-...",
  "tool_name": "Write",
  "tool_input": { "file_path": "src/auth/login.rs", "content": "..." },
  "tool_response": { "type": "text", "text": "File written" }
}
```

`saw hook` → parse stdin → forward to daemon via Unix socket at
`~/.saw/<sessionId>.sock` → sub-10ms latency.

Fallback when hooks not configured: `notify` crate tails JSONL file (~100ms latency).

### 5.4 Process Metrics via sysinfo

On Linux, `/proc/<pid>/stat` provides CPU ticks. `sysinfo` crate abstracts this
cross-platform (Linux/macOS/Windows).

Key metrics to track:
```
cpu_usage_percent    → compute over 2-second window
rss_bytes            → resident memory
io_read_bytes        → disk reads (backup activity signal)
io_write_bytes       → disk writes (backup activity signal)
```

**Stuck detection threshold:**
- CPU < 1% for 120 seconds AND no file events AND no new JSONL records → API Hang

**Loop detection via IO:**
- io_write_bytes increasing steadily → writes happening (check against file events)

### 5.4 File System Events via notify crate

The `notify` crate (v6) provides cross-platform file watching:
- Linux: inotify
- macOS: FSEvents (or kqueue)
- Windows: ReadDirectoryChangesW

Events relevant to saw:
```rust
EventKind::Modify(ModifyKind::Data(_))  // file written
EventKind::Create(_)                     // new file created
EventKind::Remove(_)                     // file deleted
```

**Watch paths:**
1. `<project_dir>/**` — catch scope leaks
2. `~/.claude/projects/<slug>/` — catch new JSONL records (inotify on dir)

**Noise filter — ignore:**
```rust
fn should_ignore(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s.contains("/.git/")
        || s.contains("/target/")
        || s.contains("/node_modules/")
        || s.contains("/__pycache__/")
        || s.contains("/.saw/")          // saw's own state dir
        || s.contains("/.claude/")         // Claude Code's own state dir
}
```

### 5.5 PID Detection — Improved with sessions/

**Primary:** read `~/.claude/sessions/` — zero guessing, exact match.

```rust
fn find_claude_pids(home: &Path) -> Vec<SessionFile> {
    // All PIDs that have a sessions file = all running or recently-run Claude sessions
    let sessions_dir = home.join(".claude/sessions");
    fs::read_dir(&sessions_dir).into_iter().flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension() == Some(OsStr::new("json")))
        .filter_map(|e| {
            let s = fs::read_to_string(e.path()).ok()?;
            serde_json::from_str::<SessionFile>(&s).ok()
        })
        // Filter to only actually-alive processes
        .filter(|s| {
            let mut sys = System::new();
            sys.refresh_process(Pid::from_u32(s.pid));
            sys.process(Pid::from_u32(s.pid)).is_some()
        })
        .collect()
}
```

**Fallback:** sysinfo scan by process name "claude".

### 5.6 Token Activity Detection — Real Method

**Verified:** `assistant` records in JSONL contain `message.usage`:

```json
{
  "type": "assistant",
  "message": {
    "usage": {
      "input_tokens": 18432,
      "output_tokens": 205,
      "cache_read_input_tokens": 0,
      "cache_creation_input_tokens": 0
    }
  }
}
```

Track cumulative totals — delta between records = rate:

```rust
struct TokenTracker {
    last_input: u64,
    last_output: u64,
    last_seen_at: Instant,
}

impl TokenTracker {
    fn update(&mut self, input: u64, output: u64) -> TokenDelta {
        let delta = TokenDelta {
            input: input.saturating_sub(self.last_input),
            output: output.saturating_sub(self.last_output),
            elapsed: self.last_seen_at.elapsed(),
        };
        self.last_input = input;
        self.last_output = output;
        self.last_seen_at = Instant::now();
        delta
    }

    fn is_frozen(&self, threshold: Duration) -> bool {
        self.last_seen_at.elapsed() > threshold
    }
}
```

**Signal:** `token_tracker.is_frozen(Duration::from_secs(120))` → strong ApiHang signal.

### 5.7 TaskBlocked — New Stuck Type from tasks/

**No exit_code** means TestLoop needs content analysis. But there's a better signal
for "agent stuck because dependencies not met":

```rust
// Detected when:
// 1. Agent session is in_progress
// 2. Corresponding task has non-empty blockedBy
// 3. All blocking tasks are NOT completed
AgentPhase::TaskBlocked {
    task_id: String,
    blocked_by: Vec<String>,
}
```

This is a false-positive-free signal — pure data from `tasks/*.json`, no heuristics.

---

## 6. Architecture

### 6.1 High-Level Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        Claude Code                               │
│                                                                  │
│   PostToolUse hook ──────────────────────────────────────────►  │
│   PreToolUse hook  ──────────────────────────────────────────►  │
│   SessionStart hook ─────────────────────────────────────────►  │
│                                                                  │
│   ~/.claude/projects/<hash>/session-<uuid>.jsonl (written live) │
└─────────────────────────────────────────────────────────────────┘
         │ hook (stdin)          │ inotify (fallback)
         ▼                       ▼
┌────────────────────────────────────────────────────────────────┐
│                     saw-daemon                                │
│                                                                  │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────────────┐ │
│  │  hook.rs     │  │  watcher.rs  │  │  process.rs          │ │
│  │  (stdin)     │  │  (notify)    │  │  (sysinfo /proc)     │ │
│  └──────┬───────┘  └──────┬───────┘  └──────────┬───────────┘ │
│         └─────────────────┼──────────────────────┘             │
│                            ▼                                    │
│                   ┌──────────────────┐                         │
│                   │  event_bus.rs    │  tokio mpsc channel     │
│                   │  AgentEvent enum │                         │
│                   └────────┬─────────┘                         │
│                            ▼                                    │
│                   ┌──────────────────┐                         │
│                   │  classifier.rs   │                         │
│                   │  StuckType enum  │                         │
│                   └────────┬─────────┘                         │
│                            ▼                                    │
│                   ┌──────────────────┐                         │
│                   │  state.rs        │  AgentState struct      │
│                   │  SQLite (tiny)   │  in-memory + persist    │
│                   └────────┬─────────┘                         │
│                            ▼                                    │
│                   ┌──────────────────┐                         │
│                   │  alerter.rs      │                         │
│                   │  bell/notify/    │                         │
│                   │  kill/webhook    │                         │
│                   └──────────────────┘                         │
└────────────────────────────────────────────────────────────────┘
         │                   │                   │
         ▼                   ▼                   ▼
┌──────────────┐   ┌──────────────┐   ┌──────────────────────┐
│ saw watch  │   │  saw tui   │   │  saw status        │
│ (plain log)  │   │  (ratatui)   │   │  (one-shot JSON/text)│
└──────────────┘   └──────────────┘   └──────────────────────┘
```

### 6.2 Event Flow

```
Claude Code executes Write tool
         │
         ├─── Hook mode: sends JSON to stdin of `saw hook`
         │              └─► saw-daemon event_bus (< 10ms latency)
         │
         └─── Fallback mode: writes to JSONL file
                            └─► inotify detects new bytes
                                └─► saw reads new lines
                                    └─► saw-daemon event_bus (~100ms latency)

event_bus receives AgentEvent::ToolCall { name: "Write", file: "src/auth/login.rs" }
         │
         ▼
classifier.update_state(&event)
         │
         ├─── update file_frequency["src/auth/login.rs"] += 1
         ├─── check scope: src/auth/login.rs matches --guard src/auth/ ✓
         ├─── check loop_score: file_frequency.max() = 2 (below threshold 3)
         └─── classify() → Working

         [2 minutes later, no events]

classifier receives tick from 5-second timer
         │
         ├─── silence_duration = 127 seconds
         ├─── cpu_usage = 0.2%
         ├─── token_rate = 0 (no new JSONL records)
         └─── classify() → ApiHang { since: 127s }
                  │
                  ▼
         alerter.alert(ApiHang { since: 127s })
                  │
                  ├─── print: "⚠ API HANG detected (2m 7s) — tokens frozen"
                  ├─── print: "  Suggestion: send follow-up message or Ctrl+C"
                  └─── (if --on-stuck=bell) → terminal bell
```

### 6.3 State Machine

```rust
pub enum AgentPhase {
    Initializing,           // Session just started
    Working,                // Active: file events + token activity
    Thinking,               // No file events, but tokens moving (< 90s)
    ApiHang(Duration),      // No activity > 120s, tokens frozen
    ToolLoop {              // Same file written repeatedly
        file: PathBuf,
        count: u32,
        since: Instant,
    },
    TestLoop {              // Bash tool, content analysis shows repeated failures
        command: String,
        failure_count: u32,
        // NOTE: no exit_code available — detected via is_error + stderr + content patterns
    },
    TaskBlocked {           // Task has unresolved blockedBy dependencies
        task_id: String,    // NEW: pure data signal, zero false positives
        blocked_by: Vec<String>,
    },
    ContextReset,           // compact_boundary or system compaction seen
    ScopeLeaking {          // File outside guard path modified
        violating_file: PathBuf,
        guard_path: PathBuf,
    },
    Idle(Duration),         // Long silence, session may be complete
    Dead,                   // Process not found in sessions/ or sysinfo
}
```

Transitions:

```
Initializing → Working          (first tool call received)
Working → Thinking              (no file events for 30s, tokens still moving)
Thinking → Working              (file event or tool call received)
Thinking → ApiHang              (silence > 120s, tokens frozen)
Working → ToolLoop              (same file written 3+ times in 5min)
Working → ScopeLeaking          (file outside guard path modified)
Working → ContextReset          (compact_boundary record seen)
ContextReset → Working          (next tool call after compact)
Any → Idle                      (silence > 10 minutes)
Any → Dead                      (PID not found)
ApiHang → Working               (new event received — SSE reconnected)
ToolLoop → Working              (different file written)
```

---

## 7. Data Sources

### 7.1 JSONL Session Log

**Location:** `~/.claude/projects/<project-hash>/session-<uuid>.jsonl`

**Project hash:** SHA-256 of the absolute project path, base64url encoded, first 20 chars.

**Finding the right file:**
```rust
fn find_session_file(project_dir: &Path) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    let projects_dir = home.join(".claude/projects");

    // Hash the project path
    let hash = sha256_base64url(project_dir.to_str()?);
    let project_cache = projects_dir.join(&hash[..20]);

    // Find most recently modified .jsonl file
    fs::read_dir(&project_cache).ok()?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension() == Some(OsStr::new("jsonl")))
        .max_by_key(|e| e.metadata().ok()?.modified().ok()?)
        .map(|e| e.path())
}
```

**Tailing strategy:** Use `notify` to watch for `ModifyKind::Data` events on the file.
When new bytes are detected, read from last known position (maintain byte offset in
`AgentState`). Parse each complete line (newline-terminated) as JSON.

**Robustness:** Lines may be partially written during high-frequency tool use. Buffer
incomplete lines until newline received. Ignore lines that fail JSON parsing (log debug).

### 7.2 Process Metrics

**Collected every 2 seconds via sysinfo:**
```rust
struct ProcessSnapshot {
    timestamp: Instant,
    cpu_percent: f32,       // 2-second window average
    rss_bytes: u64,
    virtual_bytes: u64,
    io_read_bytes: u64,     // cumulative
    io_write_bytes: u64,    // cumulative
}
```

CPU delta calculated between snapshots:
```rust
fn cpu_percent(prev: &ProcessSnapshot, curr: &ProcessSnapshot) -> f32 {
    // sysinfo handles this via refresh_process()
    // but we also compute delta manually as sanity check
}
```

### 7.3 File System Events

**Watch paths (configurable):**
```
<project_dir>/**           → catch all file modifications
~/.claude/projects/<hash>/ → catch new JSONL records
```

**Event filtering:**
- Ignore: `.git/`, `target/`, `node_modules/`, `__pycache__/`
- Ignore: events generated by saw itself (`.saw/` directory)
- Ignore: read events on large files (too noisy)

**Scope guard:**
When `--guard <path>` is set, any `Write`/`Create`/`Delete` event outside the guard path
triggers `ScopeLeaking` state transition.

### 7.4 Hook Payload

When `saw setup` has been run, the hook payload arrives on stdin of `saw hook`:

```rust
#[derive(Deserialize)]
struct HookPayload {
    hook_event_name: String,   // "PostToolUse", "PreToolUse", "SessionStart"
    session_id: String,
    tool_name: Option<String>,
    tool_input: Option<serde_json::Value>,
    tool_response: Option<serde_json::Value>,
    usage: Option<UsageInfo>,
}

#[derive(Deserialize)]
struct UsageInfo {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
}
```

`saw hook` parses this and sends a `AgentEvent` to the daemon via Unix domain socket
at `~/.saw/<session-id>.sock`.

---

## 8. Core Algorithms

### 8.1 Stuck Detection — Main Classifier

```rust
pub fn classify(state: &AgentState, now: Instant) -> AgentPhase {
    // 1. Dead check (highest priority)
    if !state.process_alive {
        return AgentPhase::Dead;
    }

    let silence = now - state.last_event_at;
    let cpu = state.latest_cpu_percent;
    let loop_score = compute_loop_score(&state.recent_tool_calls);
    let test_loop = detect_test_loop(&state.recent_tool_calls);

    // 2. Active scope violation
    if let Some(file) = &state.latest_scope_violation {
        return AgentPhase::ScopeLeaking {
            violating_file: file.clone(),
            guard_path: state.guard_path.clone().unwrap(),
        };
    }

    // 3. Context reset
    if state.last_event_was_compact {
        return AgentPhase::ContextReset;
    }

    // 4. Tool loop (file rewrite loop)
    if loop_score.file_rewrites >= 3 {
        return AgentPhase::ToolLoop {
            file: loop_score.most_written_file.clone(),
            count: loop_score.file_rewrites,
            since: state.loop_started_at.unwrap_or(now),
        };
    }

    // 5. Test loop
    if let Some(cmd) = test_loop {
        return AgentPhase::TestLoop {
            command: cmd,
            failure_count: loop_score.consecutive_test_failures,
        };
    }

    // 6. API hang — silence + no CPU + tokens frozen
    if silence > Duration::from_secs(120) && cpu < 1.0 {
        return AgentPhase::ApiHang(silence);
    }

    // 7. Extended idle (session may be complete)
    if silence > Duration::from_secs(600) {
        return AgentPhase::Idle(silence);
    }

    // 8. Thinking (recent activity, no file events, not long enough to be hang)
    if silence > Duration::from_secs(30) && silence < Duration::from_secs(120) {
        return AgentPhase::Thinking;
    }

    // 9. Working
    AgentPhase::Working
}
```

### 8.2 Loop Score — Revised for Real Data

**⚠️ No `exit_code` in JSONL.** TestLoop detection uses alternative signals:

```rust
fn is_bash_failure(record: &JsonlRecord) -> bool {
    // Signal 1: is_error flag on tool_result content item
    if let Some(is_err) = record.tool_result_is_error() {
        return is_err;
    }
    // Signal 2: non-empty stderr in toolUseResult envelope
    if let Some(ref result) = record.tool_use_result {
        if !result.stderr.as_deref().unwrap_or("").is_empty() {
            return true;
        }
        // Signal 3: interrupted
        if result.interrupted == Some(true) {
            return true;
        }
    }
    // Signal 4: content analysis for common failure patterns
    if let Some(content) = record.tool_result_content() {
        return content.contains("FAILED")
            || content.contains("error[E")
            || content.contains("panicked at")
            || content.contains("test result: FAILED")
            || content.contains("ERRORS")     // pytest
            || content.contains("npm ERR!");  // npm
    }
    false
}
```

**Token rate** — use `message.usage` from `assistant` records:
```rust
// assistant record → message.usage.input_tokens + output_tokens
// sum delta between records → tokens per minute
fn extract_token_usage(record: &JsonlRecord) -> Option<TokenUsage> {
    // Only assistant records have usage
    if record.record_type != "assistant" { return None; }
    let usage = record.message.as_ref()?.usage.as_ref()?;
    Some(TokenUsage {
        input: usage.input_tokens.unwrap_or(0),
        output: usage.output_tokens.unwrap_or(0),
    })
}
```

**Subagent awareness** — `isSidechain: true` records are from subagents:
```rust
// Weight subagent activity lower — they run in parallel, their silence
// doesn't mean the parent session is stuck
fn activity_weight(record: &JsonlRecord) -> f32 {
    if record.is_sidechain == Some(true) { 0.3 } else { 1.0 }
}
```

### 8.3 Silence Duration — Robust Calculation

```rust
fn compute_silence(state: &AgentState, now: Instant) -> Duration {
    // Take most recent of: JSONL record, file event, hook event
    let last_jsonl = state.last_jsonl_record_at;
    let last_file = state.last_file_event_at;
    let last_hook = state.last_hook_event_at;

    let last_any = [last_jsonl, last_file, last_hook]
        .iter()
        .flatten()
        .copied()
        .max()
        .unwrap_or(state.session_started_at);

    now - last_any
}
```

### 8.4 Checkpoint — Save Progress Before Kill

When `--on-stuck=checkpoint-and-kill` is set:

```rust
async fn save_checkpoint(state: &AgentState, project_dir: &Path) -> Result<PathBuf> {
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let checkpoint_dir = project_dir.join(format!(".saw/checkpoints/{}", ts));
    fs::create_dir_all(&checkpoint_dir).await?;

    // Save list of recently modified files
    let manifest: Vec<&Path> = state.recently_modified_files
        .iter()
        .map(|e| e.path.as_path())
        .collect();

    // Copy each modified file to checkpoint
    for file in &manifest {
        let relative = file.strip_prefix(project_dir)?;
        let dest = checkpoint_dir.join(relative);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::copy(file, dest).await?;
    }

    // Save state snapshot
    let state_json = serde_json::to_string_pretty(state)?;
    fs::write(checkpoint_dir.join("saw-state.json"), state_json).await?;

    // Save JSONL snapshot
    if let Some(jsonl_path) = &state.session_jsonl_path {
        fs::copy(jsonl_path, checkpoint_dir.join("session-snapshot.jsonl")).await?;
    }

    println!("[saw] Checkpoint saved → {}", checkpoint_dir.display());
    Ok(checkpoint_dir)
}
```

---

## 9. Crate Structure

```
saw/
├── Cargo.toml                    # workspace
├── Cargo.lock
├── rust-toolchain.toml           # stable, no nightly needed
├── .github/
│   └── workflows/
│       ├── ci.yml                # test + clippy + fmt
│       └── release.yml           # cross-compile + GitHub Release
├── install.sh                    # curl | bash installer
├── README.md
├── AGENTS.md                     # AI coding agent instructions
├── PLAN.md                       # this file
├── .beads/
│   └── beads.jsonl               # task tracking
│
├── crates/
│   │
│   ├── saw-core/               # types, classifier, state — no I/O
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── types.rs          # AgentEvent, AgentPhase, AgentState, ToolCall
│   │       ├── classifier.rs     # classify() pure function
│   │       ├── loop_detector.rs  # compute_loop_score()
│   │       ├── session.rs        # parse JSONL records → AgentEvent
│   │       └── metrics.rs        # token_rate, silence_duration calculations
│   │
│   ├── saw-daemon/             # background process, event aggregation
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── main.rs           # daemon entry — tokio runtime
│   │       ├── hook_server.rs    # Unix domain socket server, receives from saw hook
│   │       ├── watcher.rs        # notify crate file watching
│   │       ├── process_monitor.rs # sysinfo polling loop (every 2s)
│   │       ├── jsonl_tail.rs     # tail JSONL file, parse new records
│   │       ├── event_bus.rs      # tokio mpsc, fan-out to subscribers
│   │       ├── state_machine.rs  # apply events to AgentState, run classifier
│   │       ├── alerter.rs        # action on phase transitions
│   │       └── store.rs          # rusqlite persistence (checkpoint, history)
│   │
│   └── saw-cli/                # user-facing binary
│       ├── Cargo.toml
│       └── src/
│           ├── main.rs
│           ├── cmd/
│           │   ├── watch.rs      # plain log streaming mode
│           │   ├── tui.rs        # ratatui dashboard
│           │   ├── hook.rs       # called by Claude Code hooks, forward to daemon
│           │   ├── status.rs     # one-shot status check
│           │   ├── setup.rs      # inject hooks into .claude/settings.json
│           │   └── config.rs     # read/write ~/.config/saw/config.toml
│           └── tui/
│               ├── app.rs        # App struct, event loop
│               ├── state.rs      # TUI-specific render state
│               └── widgets/
│                   ├── status_bar.rs
│                   ├── file_activity.rs
│                   ├── metrics_panel.rs
│                   └── alerts_panel.rs
```

### 9.1 Dependency Manifest

```toml
# saw/Cargo.toml (workspace)
[workspace]
members = ["crates/saw-core", "crates/saw-daemon", "crates/saw-cli"]
resolver = "2"

[workspace.dependencies]
# Async runtime
tokio = { version = "1", features = ["full"] }

# Serialization
serde = { version = "1", features = ["derive"] }
serde_json = "1"

# Time
chrono = { version = "0.4", features = ["serde"] }

# File watching
notify = "6"

# Process metrics
sysinfo = "0.30"

# CLI
clap = { version = "4", features = ["derive", "env"] }

# TUI
ratatui = "0.26"
crossterm = "0.27"

# Persistence
rusqlite = { version = "0.31", features = ["bundled"] }

# Config
toml = "0.8"
dirs = "5"

# Error handling
anyhow = "1"
thiserror = "1"

# Logging
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

# IPC
tokio-util = { version = "0.7", features = ["codec"] }

# Total external deps: 15
# No: tree-sitter, fastembed, petgraph, git2
# Estimated binary size: ~4MB stripped
```

### 9.2 saw-core — Zero I/O, Fully Testable

`saw-core` has no I/O dependencies. All types are pure data, all functions are pure.
This means the entire classifier, loop detector, and state machine can be tested without
mocking filesystems or processes.

```rust
// saw-core/src/types.rs

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub timestamp: DateTime<Utc>,
    pub tool_name: String,
    pub file_path: Option<PathBuf>,
    pub command: Option<String>,
    pub exit_code: Option<i32>,
}

#[derive(Debug, Clone)]
pub struct AgentState {
    pub session_id: String,
    pub session_started_at: Instant,
    pub last_event_at: Instant,
    pub last_jsonl_record_at: Option<Instant>,
    pub last_file_event_at: Option<Instant>,
    pub last_hook_event_at: Option<Instant>,
    pub last_event_was_compact: bool,
    pub process_alive: bool,
    pub latest_cpu_percent: f32,
    pub latest_rss_bytes: u64,
    pub recent_tool_calls: VecDeque<ToolCall>,  // max 50
    pub recently_modified_files: VecDeque<FileEvent>, // max 20
    pub guard_path: Option<PathBuf>,
    pub latest_scope_violation: Option<PathBuf>,
    pub loop_started_at: Option<Instant>,
    pub session_jsonl_path: Option<PathBuf>,
    pub phase: AgentPhase,
}
```

---

## 10. CLI Surface

### 10.1 saw watch

```
saw watch [OPTIONS]

Streaming log mode — print status updates to stdout.

OPTIONS:
    --pid <PID>              Explicit PID to monitor (auto-detect if omitted)
    --dir <DIR>              Project directory to watch [default: .]
    --guard <PATH>           Alert on files modified outside this path
    --timeout <DURATION>     Alert after this silence duration [default: 3m]
    --on-stuck <ACTION>      Action on stuck detection: warn|bell|kill|checkpoint-kill
                             [default: warn]
    --on-scope-leak <ACTION> Action on scope leak: warn|bell|kill [default: warn]
    --poll-interval <SECS>   Process poll interval [default: 2]
    --robot                  Emit JSON objects (one per event) instead of human text
    --quiet                  Only print alerts, not status updates
    --no-color               Disable ANSI colors

EXAMPLES:
    saw watch
    saw watch --guard src/auth/ --on-scope-leak bell
    saw watch --timeout 5m --on-stuck kill
    saw watch --robot | jq 'select(.phase == "ApiHang")'
```

### 10.2 saw tui

```
saw tui [OPTIONS]

Interactive TUI dashboard with live updates.

OPTIONS:
    --pid <PID>    Explicit PID [auto-detect]
    --dir <DIR>    Project directory [default: .]
    --guard <PATH> Scope guard path
    --refresh <MS> Refresh interval in milliseconds [default: 500]

KEYBINDINGS:
    q / Ctrl+C     Quit
    k              Send SIGINT to agent (interrupt)
    K              Send SIGKILL to agent (force kill)
    c              Create checkpoint (snapshot current state)
    g              Set/update guard path
    ?              Show help
```

### 10.3 saw status

```
saw status [OPTIONS]

One-shot status check — prints current state and exits.

OPTIONS:
    --pid <PID>   Explicit PID [auto-detect]
    --json        Output JSON

EXIT CODES:
    0    Working or Thinking (agent active)
    1    ApiHang or Dead (agent stuck or gone)
    2    ToolLoop or TestLoop (agent looping)
    3    ScopeLeaking (agent modifying out-of-scope files)
    4    Idle (agent inactive for >10 min)

EXAMPLES:
    saw status
    → ● WORKING  src/auth/login.rs (3s ago)  cpu: 12%

    saw status --json
    → {"phase":"Working","last_file":"src/auth/login.rs","silence_secs":3,"cpu_percent":12.4}

    # Use in shell scripts
    if ! saw status; then
        echo "Agent not working, check status"
    fi
```

### 10.4 saw setup

```
saw setup [OPTIONS]

Inject saw hooks into Claude Code settings.

OPTIONS:
    --global     Inject into ~/.claude/settings.json (affects all projects)
    --local      Inject into .claude/settings.json (current project only) [default]
    --remove     Remove saw hooks from settings
    --dry-run    Show what would be changed without modifying

EXAMPLES:
    saw setup
    saw setup --global
    saw setup --remove
```

### 10.5 saw hook

```
saw hook [OPTIONS]

Internal command — called by Claude Code hooks machinery.
Receives hook payload on stdin, forwards to saw daemon.
Not intended for direct use.

OPTIONS:
    --pre            Hook is PreToolUse (not PostToolUse)
    --session-start  Hook is SessionStart
```

### 10.6 saw config

```
saw config [OPTIONS]

View or set configuration values in ~/.config/saw/config.toml

OPTIONS:
    --list                    Show all config values
    --timeout <DURATION>      Default stuck timeout
    --on-stuck <ACTION>       Default action on stuck
    --on-scope-leak <ACTION>  Default action on scope leak
    --guard <PATH>            Default guard path (relative to project)
    --reset                   Reset to defaults
```

---

## 11. TUI Design

### 11.1 Layout

```
┌─ VIGIL ──────────────────────────────────────────────── v0.1.0 ─┐
│ claude (pid 42891)    ● WORKING    6m 14s    [q]uit  [k]ill  [?]│
├──────────────────────────────────┬──────────────────────────────┤
│ FILE ACTIVITY                    │ METRICS                       │
│                                  │                               │
│  2s  ✎ src/auth/login.rs  +47L  │ Tokens    ↑ active           │
│  8s  ✎ src/auth/mod.rs    +3L   │ CPU       ████░░░░  12%      │
│ 34s  ○ src/utils/crypto.rs      │ Memory    342 MB              │
│ 45s  ✎ tests/auth_test.rs +12L  │ I/O       ↑1.2KB/s           │
│ 1m   ✎ src/auth/login.rs  (2)   │                               │
│                                  │ Session   6m 14s              │
│                                  │ Tool calls 23                 │
│                                  │ Files     4 touched           │
├──────────────────────────────────┴──────────────────────────────┤
│ GUARD: src/auth/   ✓ No violations                               │
├─────────────────────────────────────────────────────────────────┤
│ ALERTS                                                           │
│ ✓ No anomalies detected                                          │
└─────────────────────────────────────────────────────────────────┘
```

### 11.2 Stuck State Display

```
┌─ VIGIL ──────────────────────────────────────────────── v0.1.0 ─┐
│ claude (pid 42891)  ⚠ API HANG   4m 12s    [q]uit  [k]ill  [?] │
├──────────────────────────────────┬──────────────────────────────┤
│ FILE ACTIVITY                    │ METRICS                       │
│                                  │                               │
│ 4m12s  ✎ src/auth/login.rs      │ Tokens    ✗ frozen (4m 12s)  │
│        (last activity)           │ CPU       ░░░░░░░░  0.1%     │
│                                  │ Memory    341 MB (flat)       │
│                                  │ I/O       0 B/s               │
├──────────────────────────────────┴──────────────────────────────┤
│ ⚠ TYPE: API Hang — SSE connection may have dropped              │
│                                                                  │
│ RECOMMENDED ACTIONS:                                             │
│   1. Send a follow-up message (kicks SSE reconnect)             │
│   2. Press [k] to send SIGINT, then resume session              │
│   3. Press [K] to force kill (use --checkpoint to save state)   │
└─────────────────────────────────────────────────────────────────┘
```

### 11.3 Robot Mode Output (--robot)

JSON objects emitted to stdout, one per significant event:

```json
{"event":"phase_change","from":"Working","to":"ApiHang","since_secs":127,"timestamp":"2026-03-21T10:47:23Z","pid":42891,"session_id":"d8af951f-..."}
{"event":"phase_change","from":"ApiHang","to":"Working","resolved_after_secs":45,"timestamp":"2026-03-21T10:48:08Z"}
{"event":"scope_leak","file":"src/billing/stripe.rs","guard":"src/auth/","timestamp":"2026-03-21T10:52:11Z"}
{"event":"tool_loop","file":"src/auth/login.rs","count":4,"since_secs":180,"timestamp":"2026-03-21T10:55:00Z"}
{"event":"context_reset","timestamp":"2026-03-21T11:02:30Z","summary_length":1243}
{"event":"heartbeat","phase":"Working","silence_secs":3,"cpu_percent":12.4,"timestamp":"2026-03-21T11:05:00Z"}
```

---

## 12. Implementation Plan — Phase by Phase

### Phase 0 — Research & Setup (Day 1)

**Goal:** Validate data sources work as expected before writing production code.

Tasks:
- [ ] Run Claude Code on a test project, inspect `~/.claude/projects/` structure
- [ ] Verify JSONL format matches schema above — check field names, types
- [ ] Verify `notify` crate detects JSONL file changes in real-time on WSL (inotify)
- [ ] Verify `sysinfo` can find `claude` process by name on Linux
- [ ] Run `saw setup --dry-run` mentally — check `.claude/settings.json` format
- [ ] Create repo: `cargo new --name saw`, setup workspace with 3 crates
- [ ] Add `AGENTS.md`, `.beads/beads.jsonl`
- [ ] Add `rust-toolchain.toml` (stable)

**Deliverable:** Empty workspace that compiles. Research notes in `RESEARCH.md`.

---

### Phase 1 — Core Signal Pipeline (Days 2-5)

**Goal:** `saw watch` can detect ApiHang (Type A stuck) and print alert.

#### Day 2 — saw-core types + session parser

```rust
// saw-core/src/types.rs — all types
// saw-core/src/session.rs — parse JSONL lines
```

- Define `AgentEvent` enum: `ToolCall`, `ToolResult`, `CompactBoundary`, `SessionStart`, `UserMessage`
- Define `AgentState` struct
- Define `AgentPhase` enum
- Implement `SessionRecord::parse(line: &str) -> Option<AgentEvent>`
- Unit tests: parse each JSONL record type, malformed JSON, partial lines

#### Day 3 — Classifier (pure function)

```rust
// saw-core/src/classifier.rs
// saw-core/src/loop_detector.rs
// saw-core/src/metrics.rs
```

- Implement `classify(&AgentState, Instant) -> AgentPhase`
- Implement `compute_loop_score(&VecDeque<ToolCall>) -> LoopScore`
- Unit tests: test each phase transition
  - Working → Thinking after 45s silence
  - Thinking → ApiHang after 130s silence + cpu=0
  - Working → ToolLoop when same file written 4x
  - Working → ContextReset on compact_boundary

#### Day 4 — saw-daemon JSONL tail + process monitor

```rust
// saw-daemon/src/jsonl_tail.rs
// saw-daemon/src/process_monitor.rs
// saw-daemon/src/event_bus.rs
```

- `JsonlTailer`: watch file with `notify`, read new bytes, parse lines, emit to channel
- `ProcessMonitor`: poll `sysinfo` every 2s, emit `AgentEvent::ProcessMetrics`
- `EventBus`: `tokio::sync::broadcast`, fan-out to multiple subscribers

#### Day 5 — saw-cli watch command (MVP)

```rust
// saw-cli/src/cmd/watch.rs
```

- Auto-detect claude PID via sysinfo
- Find JSONL file via project hash
- Wire everything together: tail + process monitor → classifier → alerter
- Plain text output: timestamp + phase + last file
- Basic alerter: print colored text to stderr for alerts

**Deliverable Phase 1:** `saw watch` runs, detects ApiHang, prints alert.
Demo: simulate hang by pausing process with `kill -STOP <pid>`.

---

### Phase 2 — All Stuck Types + Actions (Days 6-9)

**Goal:** Detect all 4 phase types. Add `--on-stuck` actions. Add `--guard`.

#### Day 6 — File system watcher integration

```rust
// saw-daemon/src/watcher.rs
```

- Watch project directory recursively with `notify`
- Filter noise: `.git/`, `target/`, `.saw/`
- Emit `AgentEvent::FileModified { path, kind }`
- Feed into `AgentState.last_file_event_at` and `recently_modified_files`
- Update classifier: file events reset Thinking→Working transition

#### Day 7 — Loop detection + TestLoop

- ToolLoop: same file written 3+ times in 5 minutes
- TestLoop: Bash tool with test command, exit code != 0, 3+ times
- Feed `exit_code` from tool_result records into `ToolCall` struct
- Parse `Bash` tool input to extract command, detect test commands

#### Day 8 — Action system + guard

```rust
// saw-daemon/src/alerter.rs
```

- `--on-stuck=warn`: print message (existing)
- `--on-stuck=bell`: print + `\x07` terminal bell
- `--on-stuck=kill`: send SIGINT to agent PID
- `--on-stuck=checkpoint-kill`: save checkpoint then SIGINT
- `--guard <path>`: scope leak detection
  - On any FileModified event outside guard path → ScopeLeaking phase
  - `--on-scope-leak=warn|bell|kill`

#### Day 9 — Checkpoint implementation

```rust
// saw-daemon/src/store.rs (checkpoint part)
```

- Save recently modified files to `.saw/checkpoints/<timestamp>/`
- Save JSONL snapshot
- Save saw state JSON
- `saw watch --checkpoint`: auto-checkpoint on stuck before kill

**Deliverable Phase 2:** All stuck types detected. Guard works. Actions work.
Demo: show ToolLoop detection on a contrived example (script that writes same file 5x).

---

### Phase 3 — Hook Mode + TUI (Days 10-14)

**Goal:** Hook integration for real-time events. TUI dashboard.

#### Day 10 — saw setup + hook server

```rust
// saw-cli/src/cmd/setup.rs
// saw-daemon/src/hook_server.rs
```

- `saw setup`: parse `.claude/settings.json`, inject hook commands, write back
- Unix domain socket server in daemon at `~/.saw/<pid>.sock`
- `saw hook`: parse stdin (hook payload), connect to socket, forward event

#### Day 11 — TUI skeleton

```rust
// saw-cli/src/tui/app.rs
// saw-cli/src/tui/state.rs
```

- Ratatui App struct with crossterm event loop
- 3-panel layout: file activity | metrics | status bar
- Keyboard handling: q, k, K, c, g, ?
- Connect to same event stream as `saw watch`

#### Day 12 — TUI widgets

```rust
// saw-cli/src/tui/widgets/
```

- `StatusBar`: agent name, PID, phase indicator with color
- `FileActivityPanel`: scrolling list of recent file events with age
- `MetricsPanel`: CPU bar, memory, token activity indicator
- `AlertsPanel`: alerts with timestamps, recommendations

#### Day 13 — Robot mode + saw status

```rust
// saw-cli/src/cmd/status.rs
```

- `saw watch --robot`: JSON objects to stdout
- Define JSON schema for all event types
- `saw status`: one-shot, exit code semantics
- `saw status --json`: JSON snapshot

#### Day 14 — Integration + polish

- Test hook mode end-to-end on real Claude Code session
- Test all 4 stuck types with real agent (contrived prompts that cause each)
- Fix any WSL-specific issues with inotify or process detection
- Add `--quiet` mode, `--no-color`

**Deliverable Phase 3:** Full feature set working. TUI polished.

---

### Phase 4 — Release (Days 15-16)

**Goal:** Ship it.

#### Day 15 — Release infrastructure

- `install.sh`: curl | bash, detect platform (Linux x86_64, macOS arm64/x86_64)
- GitHub Actions `release.yml`:
  - Trigger: push tag `v*`
  - Jobs: `x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`, `x86_64-apple-darwin`
  - Use `cross` crate for Linux builds
  - Upload binaries to GitHub Release
- README: GIF demo, installation, quick start, all commands documented

#### Day 16 — Launch

- Record demo GIF (15-30 seconds): stuck detection in action
- Write HN Show HN post
- Post to `r/ClaudeAI`, `r/rust`

---

## 13. Testing Strategy

### Unit Tests (saw-core)

All pure functions — no mocking needed.

```rust
#[cfg(test)]
mod tests {
    // Test each AgentPhase transition
    #[test] fn working_after_tool_call() { ... }
    #[test] fn thinking_after_30s_silence() { ... }
    #[test] fn api_hang_after_120s_silence_no_cpu() { ... }
    #[test] fn tool_loop_after_3_rewrites() { ... }
    #[test] fn test_loop_after_3_failures() { ... }
    #[test] fn context_reset_on_compact_boundary() { ... }
    #[test] fn scope_leak_outside_guard() { ... }
    #[test] fn no_scope_leak_inside_guard() { ... }

    // Test JSONL parsing
    #[test] fn parse_assistant_message() { ... }
    #[test] fn parse_tool_result_with_exit_code() { ... }
    #[test] fn parse_compact_boundary() { ... }
    #[test] fn ignore_malformed_json() { ... }
    #[test] fn ignore_partial_line() { ... }

    // Test loop score
    #[test] fn loop_score_single_file_rewrites() { ... }
    #[test] fn loop_score_consecutive_same_tool() { ... }
    #[test] fn loop_score_test_failures() { ... }
}
```

### Integration Tests (saw-daemon)

Use temp directories and synthetic JSONL files.

```rust
// tests/integration/jsonl_tail.rs
// Write synthetic JSONL, verify events are emitted correctly
// Test: partial line at end of file (simulate in-progress write)
// Test: file rotation (new session file created)
// Test: large file (1000+ records)
```

### End-to-End Tests

```bash
# tests/e2e/test_api_hang.sh
# Start a sleep process, monitor it with saw watch
# Verify ApiHang detected after timeout
# Verify --on-stuck=kill terminates the process

# tests/e2e/test_scope_leak.sh
# Write a file outside the guard path
# Verify ScopeLeaking alert emitted in robot mode
```

### Manual Test Scenarios

Document in `tests/MANUAL_TEST_SCENARIOS.md`:

1. **Real API hang:** Set a very long prompt, watch saw detect SSE timeout
2. **Real tool loop:** Prompt agent to fix a test that has an unfixable assertion
3. **Real scope leak:** Tell agent to "improve the codebase" with tight guard
4. **Context reset:** Run very long session to trigger compact
5. **Process kill:** Kill Claude Code mid-session, verify saw reports Dead

---

## 14. Release & Distribution

### Binary Targets

```
x86_64-unknown-linux-gnu      # Linux (most servers, WSL)
aarch64-unknown-linux-gnu     # Linux ARM (Raspberry Pi, cloud ARM)
x86_64-apple-darwin           # macOS Intel
aarch64-apple-darwin          # macOS Apple Silicon (most common)
x86_64-pc-windows-msvc        # Windows (future, Phase 5)
```

### install.sh Pattern (borrowed from linehash)

```bash
#!/usr/bin/env bash
set -euo pipefail

REPO="quangdang46/saw"
BINARY="saw"

detect_platform() {
    OS=$(uname -s)
    ARCH=$(uname -m)
    case "${OS}-${ARCH}" in
        Linux-x86_64)  echo "x86_64-unknown-linux-gnu" ;;
        Darwin-x86_64) echo "x86_64-apple-darwin" ;;
        Darwin-arm64)  echo "aarch64-apple-darwin" ;;
        *) echo "Unsupported platform: ${OS}-${ARCH}" && exit 1 ;;
    esac
}

PLATFORM=$(detect_platform)
VERSION=$(curl -sI "https://github.com/${REPO}/releases/latest" | grep location | sed 's/.*tag\///' | tr -d '\r')
URL="https://github.com/${REPO}/releases/download/${VERSION}/${BINARY}-${PLATFORM}.tar.gz"

curl -fsSL "$URL" | tar xz -C /usr/local/bin
chmod +x /usr/local/bin/saw
echo "saw ${VERSION} installed to /usr/local/bin/saw"
```

### Cargo Install (secondary)

```
cargo install saw
```

### GitHub Actions Release

```yaml
# .github/workflows/release.yml
on:
  push:
    tags: ['v*']

jobs:
  build:
    strategy:
      matrix:
        include:
          - target: x86_64-unknown-linux-gnu
            os: ubuntu-latest
            use_cross: true
          - target: aarch64-unknown-linux-gnu
            os: ubuntu-latest
            use_cross: true
          - target: x86_64-apple-darwin
            os: macos-latest
            use_cross: false
          - target: aarch64-apple-darwin
            os: macos-latest
            use_cross: false
    runs-on: ${{ matrix.os }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: ${{ matrix.target }}
      - name: Build
        run: |
          if ${{ matrix.use_cross }}; then
            cargo install cross
            cross build --release --target ${{ matrix.target }}
          else
            cargo build --release --target ${{ matrix.target }}
          fi
      - name: Package
        run: |
          cd target/${{ matrix.target }}/release
          tar czf saw-${{ matrix.target }}.tar.gz saw
      - uses: softprops/action-gh-release@v1
        with:
          files: target/${{ matrix.target }}/release/saw-${{ matrix.target }}.tar.gz
```

---

## 15. Success Metrics

### Week 1 (internal)
- `saw watch` detects ApiHang on real Claude Code session
- Zero false positives in 2 hours of normal use

### Week 2 (launch)
- 100+ GitHub stars within 48 hours of HN post
- Zero crash reports (no panics, graceful error handling everywhere)
- Works on Linux (WSL + native) and macOS

### Month 1
- 500+ stars
- At least 1 mention by a Claude Code power user on Twitter/X

### Quality gates (non-negotiable before launch)
- `cargo clippy -- -D warnings`: zero warnings
- `cargo test`: all pass
- `cargo fmt --check`: clean
- Manual test all 4 stuck types on real Claude Code
- README has working demo GIF

---

## 16. Open Questions

### Q1: JSONL location on Windows
On Windows, `~/.claude/` maps to `%APPDATA%\Claude\` or `%USERPROFILE%\.claude\`?
Slug encoding may also differ (backslash vs forward slash).
`dirs` crate + `path_to_slug` needs Windows-specific test.

### Q2: `system` record subtype values — what triggers compact?
Research found `compact_boundary` as a subtype. Are there other subtypes?
(`session_start`, `session_end`?) Need to sample more `system` records to build
complete subtype enum.

### Q3: `progress` record — what does it contain?
Observed in JSONL but schema not captured. Does it contain token counts?
Timing info? Could be useful for Thinking vs ApiHang distinction.

### Q4: WSL inotify + JSONL writes from Windows-side
If Claude Code binary is the Windows native version (not WSL), it writes
`~/.claude/` to the Windows filesystem. On WSL2, `/mnt/c/Users/<name>/.claude/`
— inotify does NOT fire for Windows-side writes. Need polling fallback:
```rust
// If notify fails to emit events within 5s despite process being alive
// → fall back to polling (stat the file every 2s, compare mtime)
```

### Q5: `queue-operation` record — agent task queue?
Observed in JSONL top-level types but schema not captured. Could be related to
`tasks/` queue operations. If it contains task IDs, could improve TaskBlocked linkage.

### Q6: Multiple concurrent sessions in same project
If two Claude sessions run in the same `cwd`, they write to the same project slug
directory but different `<session-uuid>.jsonl` files. `saw watch` without `--pid`
should pick the most recently active one (highest `startedAt` in `sessions/*.json`).

### Q7: subagent JSONL activity vs parent session
`isSidechain: true` records appear in the parent JSONL when subagents run.
But subagents also have their own `subagents/<agent>.jsonl`. Which to monitor?
Current plan: monitor parent JSONL only, weight `isSidechain` records lower.
Subagent-specific monitoring is Phase 5 scope.

---

## Appendix A: Config File Schema

```toml
# ~/.config/saw/config.toml

[defaults]
timeout = "3m"
on_stuck = "warn"          # warn | bell | kill | checkpoint-kill
on_scope_leak = "warn"     # warn | bell | kill
poll_interval_secs = 2
quiet = false

[guard]
# Optional default guard path (relative to project root)
# path = "src/"

[alerts]
bell = true                # terminal bell on alert
# webhook_url = "http://..."  # future: webhook on alert
```

---

## Appendix B: Verified Rust Data Model

All types below derived from actual on-disk data, not assumptions.

```rust
// ── sessions/<pid>.json ──────────────────────────────────────────
#[derive(Debug, Deserialize, Serialize)]
pub struct SessionFile {
    pub pid: u32,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub cwd: String,
    #[serde(rename = "startedAt")]
    pub started_at: u64,    // epoch ms
}

// ── projects/<slug>/<uuid>.jsonl (parsed events) ─────────────────
#[derive(Debug)]
pub enum JsonlEvent {
    ToolCall {
        ts: DateTime<Utc>,
        session_id: String,
        tool_id: String,
        tool_name: String,
        file_path: Option<PathBuf>,
        command: Option<String>,
        is_sidechain: bool,
    },
    ToolResult {
        ts: DateTime<Utc>,
        session_id: String,
        tool_id: String,
        is_error: Option<bool>,         // sometimes absent
        content: String,                // may be <persisted-output> pointer
        stderr: Option<String>,         // Bash only, from toolUseResult
        interrupted: Option<bool>,      // from toolUseResult
        persisted_path: Option<PathBuf>,
        is_sidechain: bool,
    },
    TokenActivity {
        ts: DateTime<Utc>,
        session_id: String,
        input_tokens: u64,
        output_tokens: u64,
        stop_reason: String,    // "tool_use" | "end_turn" | "max_tokens"
    },
    ContextCompacted {
        ts: DateTime<Utc>,
        session_id: String,
    },
    Unknown,    // progress, queue-operation, agent-name, etc — safely ignored
}

// ── tasks/<list-id>/<n>.json ─────────────────────────────────────
#[derive(Debug, Deserialize)]
pub struct TaskFile {
    pub id: String,
    pub subject: String,
    pub description: String,
    pub status: String,             // "pending" | "in_progress" | "completed"
    pub blocks: Vec<String>,        // task IDs this task blocks
    #[serde(rename = "blockedBy")]
    pub blocked_by: Vec<String>,    // task IDs blocking this task
    pub owner: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

// ── projects/<slug>/<uuid>/subagents/*.meta.json ─────────────────
#[derive(Debug, Deserialize)]
pub struct SubagentMeta {
    #[serde(rename = "agentType")]
    pub agent_type: Option<String>,     // open string — treat as label only
    pub description: Option<String>,
    #[serde(rename = "worktreePath")]
    pub worktree_path: Option<String>,
    // NOTE: no sessionId, no parentSessionId
    // Parent linkage = structural path: projects/<slug>/<PARENT-UUID>/subagents/
}

// ── teams/<name>/config.json ─────────────────────────────────────
#[derive(Debug, Deserialize)]
pub struct TeamConfig {
    pub name: String,
    pub description: String,
    #[serde(rename = "createdAt")]
    pub created_at: u64,
    #[serde(rename = "leadSessionId")]
    pub lead_session_id: String,    // links to sessions/<pid>.json
    pub members: Vec<TeamMember>,
}

#[derive(Debug, Deserialize)]
pub struct TeamMember {
    #[serde(rename = "agentId")]
    pub agent_id: String,
    pub name: String,
    #[serde(rename = "agentType")]
    pub agent_type: String,
    pub model: String,
    pub cwd: String,                // slug(cwd) → project folder
    #[serde(rename = "tmuxPaneId")]
    pub tmux_pane_id: String,
}
```

```markdown
# AGENTS.md — saw

## Project Overview
saw (Session Activity Watcher) is a Rust CLI tool for real-time AI agent observability.
It monitors Claude Code (and other AI agents) from the outside,
detecting stuck states, scope leaks, and tool loops.

## Crate Structure
- saw-core: pure types + classifier (no I/O, fully unit testable)
- saw-daemon: background event aggregation (tokio async)
- saw-cli: user-facing binary (clap + ratatui)

## Key Invariants
- saw-core MUST have zero I/O dependencies
- All classifier logic MUST be pure functions (no side effects)
- Binary size MUST remain < 5MB stripped

## Before Every Commit
1. cargo clippy -- -D warnings
2. cargo test
3. cargo fmt --check

## Testing Stuck Types
- ApiHang: `kill -STOP <pid>` then wait 3 minutes
- ToolLoop: write a script that writes the same file 4 times rapidly
- ScopeLeaking: `touch src/out-of-scope.rs` while saw --guard src/auth/ is running
```

---

*End of PLAN.md — ~1,100 lines*
*Version: 0.1.0-draft*
*Last updated: 2026-03-21*
