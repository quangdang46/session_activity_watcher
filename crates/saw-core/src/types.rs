//! Core serializable domain types for `saw-core`.
//!
//! The types in this module are intentionally pure data structures so they can be
//! shared by the parser, classifier, daemon, CLI, and checkpointing code without
//! pulling in any filesystem, process, or async dependencies.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MAX_RECENT_TOOL_CALLS: usize = 50;
const MAX_RECENTLY_MODIFIED_FILES: usize = 20;
const TOOL_LOOP_WINDOW_SECS: u64 = 5 * 60;
const SIDECHAIN_ACTIVITY_WEIGHT: f32 = 0.3;

/// High-level classification of what the agent appears to be doing.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum AgentPhase {
    /// The session has started, but there is not yet enough activity to classify it.
    #[default]
    Initializing,
    /// The agent is actively making forward progress.
    Working,
    /// The agent is active but currently reasoning instead of producing visible output.
    Thinking,
    /// A tool call appears to be stuck waiting on the API for the given duration.
    ApiHang(Duration),
    /// The same file is being rewritten repeatedly without meaningful progress.
    ToolLoop {
        /// File path being rewritten in a loop.
        file: PathBuf,
        /// Number of recent rewrites observed for the file.
        count: u32,
        /// When the repeating rewrite pattern began.
        since: DateTime<Utc>,
    },
    /// The agent is repeatedly running a failing test command.
    TestLoop {
        /// Most recent repeated test command.
        command: String,
        /// Number of consecutive failed test runs.
        failure_count: u32,
    },
    /// The active task is blocked by external dependencies or prerequisites.
    TaskBlocked {
        /// Identifier for the current in-progress task.
        task_id: String,
        /// Blocking task IDs that are not yet completed.
        blocked_by: Vec<String>,
    },
    /// A Claude compact boundary was observed and the session context was reset.
    ContextReset,
    /// The agent modified files outside the configured allowed scope.
    ScopeLeaking {
        /// File path outside the configured guard.
        violating_file: PathBuf,
        /// Guard path the session was expected to stay within.
        guard_path: PathBuf,
    },
    /// No meaningful activity has been observed for the given duration.
    Idle(Duration),
    /// The Claude session is unresponsive or the backing process is no longer alive.
    Dead,
}

/// Approximate line-level change summary extracted from editable tool inputs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolLineChange {
    /// Number of lines present in newly written content.
    pub added: u32,
    /// Number of lines replaced or removed from prior content.
    pub removed: u32,
}

/// Normalized record of a single tool invocation.
///
/// This type is stored in `AgentState::recent_tool_calls` so loop detection can work
/// from a compact, serializable event history instead of raw JSONL records.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolCall {
    /// When the tool invocation was emitted by the session log.
    pub timestamp: DateTime<Utc>,
    /// The tool name as exposed by Claude Code, such as `Read` or `Bash`.
    pub tool_name: String,
    /// The primary file path associated with the call, when one exists.
    pub file_path: Option<PathBuf>,
    /// The shell command for command-driven tools such as `Bash`, when present.
    pub command: Option<String>,
    /// Approximate line delta derived from tool input when available.
    #[serde(default)]
    pub line_change: Option<ToolLineChange>,
    /// Whether the most recently observed result for this call was an error.
    ///
    /// New calls should start as `false` and may be updated after a matching
    /// `ToolResult` event is applied.
    pub is_error: bool,
    /// Whether this tool call is expected to modify a file.
    pub is_write: bool,
    /// Whether the call came from a sidechain/subagent record.
    pub is_sidechain: bool,
}

/// Summary statistics derived from recent tool activity.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct LoopScore {
    /// Number of recent rewrites to the most frequently written file.
    pub file_rewrites: u32,
    /// File path that appears most often in the recent tool-call window.
    pub most_written_file: Option<PathBuf>,
    /// Number of consecutive failed test-oriented tool calls.
    pub consecutive_test_failures: u32,
}

/// Minimal task record loaded from `.claude/tasks/<list-id>/*.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct TaskFile {
    /// Task identifier within the task list.
    pub id: String,
    /// Task lifecycle status such as `pending`, `in_progress`, or `completed`.
    pub status: String,
    /// Claimed owner name when the task list belongs to a multi-agent team.
    pub owner: Option<String>,
    /// Task IDs that must complete before this task is unblocked.
    #[serde(rename = "blockedBy")]
    pub blocked_by: Vec<String>,
}

/// File-system change kind tracked by the watcher layer and tool activity parser.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum FileChangeKind {
    /// A file was read without modification.
    Read,
    /// A file was created.
    Created,
    /// A file's contents or metadata changed.
    Modified,
    /// A file was deleted.
    Deleted,
}

/// Serializable description of a single file activity event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileModification {
    /// When the file event was observed.
    pub timestamp: DateTime<Utc>,
    /// File path associated with the event.
    pub path: PathBuf,
    /// What kind of file change occurred.
    pub kind: FileChangeKind,
    /// Approximate line delta extracted from tool input, when available.
    #[serde(default)]
    pub line_change: Option<ToolLineChange>,
}

/// Token-level activity reported by Claude session logs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TokenActivity {
    /// When the token event was observed.
    pub timestamp: DateTime<Utc>,
    /// Number of input tokens counted for the associated turn, when known.
    pub input_tokens: u64,
    /// Number of output tokens produced for the associated turn, when known.
    pub output_tokens: u64,
    /// Claude stop reason for the turn, such as `tool_use` or `end_turn`.
    pub stop_reason: Option<String>,
    /// Whether the token update came from a sidechain/subagent record.
    pub is_sidechain: bool,
}

/// Point-in-time process snapshot collected from a process monitor.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProcessMetrics {
    /// When the snapshot was taken.
    pub timestamp: DateTime<Utc>,
    /// Whether the process still exists at sample time.
    pub process_alive: bool,
    /// CPU usage percentage for the sampled interval.
    pub cpu_percent: f32,
    /// Resident memory size in bytes.
    pub rss_bytes: u64,
    /// Virtual memory size in bytes.
    pub virtual_bytes: u64,
    /// Cumulative bytes read from storage.
    pub io_read_bytes: u64,
    /// Cumulative bytes written to storage.
    pub io_write_bytes: u64,
    /// Read throughput in bytes per second over the sampled interval.
    pub io_read_rate: f32,
    /// Write throughput in bytes per second over the sampled interval.
    pub io_write_rate: f32,
}

/// Normalized event stream consumed by the state machine.
///
/// Most variants come directly from Claude session logs. `FileModified` and
/// `ProcessMetrics` are produced by local observers so the classifier can combine
/// user-visible activity with machine-level signals.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum AgentEvent {
    /// Session lifecycle marker carrying the canonical session identifier.
    SessionStart {
        /// Event timestamp.
        timestamp: DateTime<Utc>,
        /// Claude session identifier.
        session_id: String,
    },
    /// Plain user-authored activity that should count as recent session activity.
    UserMessage {
        /// Event timestamp.
        timestamp: DateTime<Utc>,
        /// Whether the message came from a sidechain/subagent record.
        is_sidechain: bool,
    },
    /// Tool invocation emitted by Claude Code.
    ToolCall(ToolCall),
    /// Result emitted after a tool invocation completes.
    ToolResult {
        /// Event timestamp.
        timestamp: DateTime<Utc>,
        /// Tool name when the source record provides it.
        tool_name: Option<String>,
        /// Whether the tool result represents an error.
        is_error: bool,
        /// Text content or stdout captured from the tool result, when available.
        output: Option<String>,
        /// Stderr captured from the tool result envelope, when available.
        stderr: Option<String>,
        /// Whether the tool execution was interrupted.
        interrupted: bool,
        /// Path referenced by a `<persisted-output>` pointer, when present.
        persisted_output_path: Option<PathBuf>,
        /// Whether the tool result came from a sidechain/subagent record.
        is_sidechain: bool,
    },
    /// Token accounting activity observed in the session log.
    TokenActivity(TokenActivity),
    /// File-system activity observed inside the watched project.
    FileModified(FileModification),
    /// Process snapshot observed by the process monitor.
    ProcessMetrics(ProcessMetrics),
    /// Claude compact boundary marker.
    CompactBoundary {
        /// Event timestamp.
        timestamp: DateTime<Utc>,
        /// Whether the compact boundary came from a sidechain/subagent record.
        is_sidechain: bool,
    },
}

impl AgentEvent {
    /// Returns the canonical timestamp for this event.
    pub fn timestamp(&self) -> DateTime<Utc> {
        match self {
            Self::SessionStart { timestamp, .. }
            | Self::UserMessage { timestamp, .. }
            | Self::ToolResult { timestamp, .. }
            | Self::CompactBoundary { timestamp, .. } => *timestamp,
            Self::ToolCall(call) => call.timestamp,
            Self::TokenActivity(activity) => activity.timestamp,
            Self::FileModified(event) => event.timestamp,
            Self::ProcessMetrics(metrics) => metrics.timestamp,
        }
    }
}

/// Serializable snapshot of all state needed to classify a session.
///
/// The struct stores raw observations rather than derived conclusions wherever
/// possible. That keeps it useful for checkpoints, tests, and future classifiers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AgentState {
    /// Current Claude session identifier, once known.
    pub session_id: Option<String>,
    /// Cumulative input tokens observed across assistant turns.
    pub total_input_tokens: u64,
    /// Cumulative output tokens observed across assistant turns.
    pub total_output_tokens: u64,
    /// Total number of tool calls observed during the session.
    pub total_tool_calls: u64,
    /// Unique files touched during the session.
    pub touched_files: HashSet<PathBuf>,
    /// When the current session started.
    pub session_started_at: Option<DateTime<Utc>>,
    /// Most recent meaningful activity timestamp.
    ///
    /// This intentionally excludes passive process polling so silence-based
    /// classifiers are not masked by monitoring traffic.
    pub last_event_at: Option<DateTime<Utc>>,
    /// Most recent event that came from the Claude session JSONL stream.
    pub last_jsonl_record_at: Option<DateTime<Utc>>,
    /// Most recent non-sidechain event from the Claude session JSONL stream.
    pub last_main_jsonl_record_at: Option<DateTime<Utc>>,
    /// Most recent sidechain/subagent event from the Claude session JSONL stream.
    pub last_sidechain_jsonl_record_at: Option<DateTime<Utc>>,
    /// Most recent watcher-driven file modification timestamp.
    pub last_file_event_at: Option<DateTime<Utc>>,
    /// Most recent hook-originated activity timestamp, when hooks are wired in.
    pub last_hook_event_at: Option<DateTime<Utc>>,
    /// Most recent process metrics sample timestamp.
    pub last_process_metrics_at: Option<DateTime<Utc>>,
    /// Most recent time the process memory footprint changed.
    pub last_memory_activity_at: Option<DateTime<Utc>>,
    /// Most recent tool call timestamp.
    pub last_tool_call_at: Option<DateTime<Utc>>,
    /// Most recent tool result timestamp.
    pub last_tool_result_at: Option<DateTime<Utc>>,
    /// Most recent token activity timestamp.
    pub last_token_activity_at: Option<DateTime<Utc>>,
    /// Most recent compact boundary timestamp.
    pub last_compact_at: Option<DateTime<Utc>>,
    /// Number of main-session compact boundaries observed during the current session.
    pub compact_count: u64,
    /// Most recently observed tool name.
    pub last_tool_name: Option<String>,
    /// Most recently observed file path from either tool calls or file events.
    pub last_file_path: Option<PathBuf>,
    /// Most recently observed shell command, when a command-oriented tool was used.
    pub last_command: Option<String>,
    /// Number of consecutive failed test commands observed for the main session.
    pub consecutive_test_failures: u32,
    /// Most recent Bash test command used for loop detection context.
    pub last_test_command: Option<String>,
    /// File being worked on when the most recent test command ran, when known.
    pub last_test_file: Option<PathBuf>,
    /// Whether the last meaningful main-session transition was a compact boundary.
    ///
    /// This flag stays set until the next main-session tool call is observed.
    pub last_event_was_compact: bool,
    /// Whether a main-session tool call has started and no main-session result has been observed yet.
    pub awaiting_tool_result: bool,
    /// Approximate count of observed sidechain/subagent activity records.
    ///
    /// Parent-session JSONL currently exposes `isSidechain` but not a stable
    /// subagent identifier, so this is a running sidechain-activity count rather
    /// than an exact distinct-agent cardinality.
    pub subagent_count: u32,
    /// Latest known liveness status for the monitored Claude process.
    pub process_alive: bool,
    /// Most recent CPU percentage from process sampling.
    pub latest_cpu_percent: f32,
    /// Most recent resident memory measurement in bytes.
    pub latest_rss_bytes: u64,
    /// Most recent virtual memory measurement in bytes.
    pub latest_virtual_bytes: u64,
    /// Most recent cumulative storage read count in bytes.
    pub latest_io_read_bytes: u64,
    /// Most recent cumulative storage write count in bytes.
    pub latest_io_write_bytes: u64,
    /// Most recent storage read throughput in bytes per second.
    pub latest_io_read_rate: f32,
    /// Most recent storage write throughput in bytes per second.
    pub latest_io_write_rate: f32,
    /// Most recent time any non-zero I/O throughput was observed.
    pub last_io_activity_at: Option<DateTime<Utc>>,
    /// Sliding window of recent tool calls, capped to a small fixed history.
    pub recent_tool_calls: VecDeque<ToolCall>,
    /// Sliding window of recent file modifications, capped to a small fixed history.
    pub recently_modified_files: VecDeque<FileModification>,
    /// Recent watcher-observed write times per file for ToolLoop detection.
    #[serde(skip, default)]
    pub recent_file_write_times: HashMap<PathBuf, VecDeque<Instant>>,
    /// Wall-clock timestamps paired with `recent_file_write_times` for reporting when a loop started.
    #[serde(skip, default)]
    pub recent_file_write_timestamps: HashMap<PathBuf, VecDeque<DateTime<Utc>>>,
    /// Most recent file modification observed in the tool-loop window.
    #[serde(skip, default)]
    pub last_loop_file_path: Option<PathBuf>,
    /// Allowed path prefixes used for scope-leak detection.
    pub guard_paths: Vec<PathBuf>,
    /// Total number of out-of-scope file modifications observed for the current guard.
    pub scope_violation_count: u32,
    /// Most recent out-of-scope file touched by the agent, if any.
    pub latest_scope_violation: Option<PathBuf>,
    /// Timestamp marking when a suspected loop began.
    pub loop_started_at: Option<DateTime<Utc>>,
    /// Path to the backing session JSONL file, when known.
    pub session_jsonl_path: Option<PathBuf>,
    /// Claude task list identifier, when the session is associated with `.claude/tasks/<list-id>/`.
    pub task_list_id: Option<String>,
    /// Cached task files for the associated task list, keyed by task ID.
    pub task_files: HashMap<String, TaskFile>,
    /// Filesystem signature for the cached task list.
    #[serde(skip, default)]
    pub task_files_signature: Option<u64>,
    /// Current in-progress task for this session, when one can be resolved from task files.
    pub current_task: Option<TaskFile>,
    /// Most recently assigned high-level phase for the session.
    ///
    /// This is optional derived state cached alongside the raw observations so UI or
    /// checkpoint code can persist the latest classifier output.
    pub phase: AgentPhase,
}

pub(crate) fn path_matches_any_guard(path: &Path, guard_paths: &[PathBuf]) -> bool {
    guard_paths
        .iter()
        .any(|guard_path| path.starts_with(guard_path))
}

impl Default for AgentState {
    fn default() -> Self {
        Self {
            session_id: None,
            total_input_tokens: 0,
            total_output_tokens: 0,
            total_tool_calls: 0,
            touched_files: HashSet::new(),
            session_started_at: None,
            last_event_at: None,
            last_jsonl_record_at: None,
            last_main_jsonl_record_at: None,
            last_sidechain_jsonl_record_at: None,
            last_file_event_at: None,
            last_hook_event_at: None,
            last_process_metrics_at: None,
            last_memory_activity_at: None,
            last_tool_call_at: None,
            last_tool_result_at: None,
            last_token_activity_at: None,
            last_compact_at: None,
            compact_count: 0,
            last_tool_name: None,
            last_file_path: None,
            last_command: None,
            consecutive_test_failures: 0,
            last_test_command: None,
            last_test_file: None,
            last_event_was_compact: false,
            awaiting_tool_result: false,
            subagent_count: 0,
            process_alive: true,
            latest_cpu_percent: 0.0,
            latest_rss_bytes: 0,
            latest_virtual_bytes: 0,
            latest_io_read_bytes: 0,
            latest_io_write_bytes: 0,
            latest_io_read_rate: 0.0,
            latest_io_write_rate: 0.0,
            last_io_activity_at: None,
            recent_tool_calls: VecDeque::new(),
            recently_modified_files: VecDeque::new(),
            recent_file_write_times: HashMap::new(),
            recent_file_write_timestamps: HashMap::new(),
            last_loop_file_path: None,
            guard_paths: Vec::new(),
            scope_violation_count: 0,
            latest_scope_violation: None,
            loop_started_at: None,
            session_jsonl_path: None,
            task_list_id: None,
            task_files: HashMap::new(),
            task_files_signature: None,
            current_task: None,
            phase: AgentPhase::Initializing,
        }
    }
}

impl AgentState {
    pub fn touched_file_count(&self) -> usize {
        self.touched_files.len()
    }

    fn track_touched_file(&mut self, path: PathBuf) {
        self.touched_files.insert(path);
    }

    fn mark_jsonl_activity(&mut self, timestamp: DateTime<Utc>, is_sidechain: bool) {
        self.last_jsonl_record_at = Some(timestamp);
        if is_sidechain {
            self.last_sidechain_jsonl_record_at = Some(timestamp);
            self.subagent_count = self.subagent_count.saturating_add(1);
            self.last_event_at = match self.last_event_at {
                Some(current) if current >= timestamp => Some(current),
                _ => Some(timestamp),
            };
        } else {
            self.last_main_jsonl_record_at = Some(timestamp);
            self.last_event_at = Some(timestamp);
        }
    }

    fn is_test_command(command: &str) -> bool {
        let tokens = command
            .split_whitespace()
            .map(|token| {
                token
                    .trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | ';'))
                    .to_ascii_lowercase()
            })
            .filter(|token| !token.is_empty())
            .collect::<Vec<_>>();

        contains_sequence(&tokens, &["cargo", "test"])
            || contains_sequence(&tokens, &["npm", "test"])
            || contains_sequence(&tokens, &["pytest"])
            || contains_sequence(&tokens, &["python", "-m", "pytest"])
            || contains_sequence(&tokens, &["go", "test"])
    }

    fn reset_test_loop_tracking(&mut self, clear_context: bool) {
        self.consecutive_test_failures = 0;
        if clear_context {
            self.last_test_command = None;
            self.last_test_file = None;
        }
    }

    fn prune_file_write_history(&mut self, now: Instant) {
        let window = Duration::from_secs(TOOL_LOOP_WINDOW_SECS);
        let mut empty_paths = Vec::new();

        for (path, times) in self.recent_file_write_times.iter_mut() {
            while times.front().is_some_and(|timestamp| {
                now.checked_duration_since(*timestamp)
                    .map(|elapsed| elapsed > window)
                    .unwrap_or(false)
            }) {
                times.pop_front();
            }

            if let Some(wall_times) = self.recent_file_write_timestamps.get_mut(path) {
                while wall_times.len() > times.len() {
                    wall_times.pop_front();
                }
            }

            if times.is_empty() {
                empty_paths.push(path.clone());
            }
        }

        for path in empty_paths {
            self.recent_file_write_times.remove(&path);
            self.recent_file_write_timestamps.remove(&path);
            if self.last_loop_file_path.as_ref() == Some(&path) {
                self.last_loop_file_path = None;
            }
        }
    }

    fn note_file_write(&mut self, path: &Path, timestamp: DateTime<Utc>) {
        let now = Instant::now();
        let path = path.to_path_buf();

        if self
            .last_loop_file_path
            .as_ref()
            .is_some_and(|last| last != &path)
        {
            self.recent_file_write_times.clear();
            self.recent_file_write_timestamps.clear();
            self.loop_started_at = None;
        }

        self.prune_file_write_history(now);

        self.recent_file_write_times
            .entry(path.clone())
            .or_default()
            .push_back(now);
        self.recent_file_write_timestamps
            .entry(path.clone())
            .or_default()
            .push_back(timestamp);
        self.last_loop_file_path = Some(path.clone());

        if let Some(times) = self.recent_file_write_times.get(&path) {
            self.loop_started_at = if times.len() >= 3 {
                self.recent_file_write_timestamps
                    .get(&path)
                    .and_then(|timestamps| timestamps.front().copied())
            } else {
                None
            };
        }
    }

    pub fn recent_file_write_count(&self, path: &Path) -> u32 {
        let now = Instant::now();
        let window = Duration::from_secs(TOOL_LOOP_WINDOW_SECS);

        self.recent_file_write_times
            .get(path)
            .map(|times| {
                times
                    .iter()
                    .filter(|timestamp| {
                        now.checked_duration_since(**timestamp)
                            .map(|elapsed| elapsed <= window)
                            .unwrap_or(false)
                    })
                    .count() as u32
            })
            .unwrap_or(0)
    }

    pub fn recent_file_write_started_at(&self, path: &Path) -> Option<DateTime<Utc>> {
        let now = Instant::now();
        let window = Duration::from_secs(TOOL_LOOP_WINDOW_SECS);
        let instants = self.recent_file_write_times.get(path)?;
        let timestamps = self.recent_file_write_timestamps.get(path)?;

        instants
            .iter()
            .zip(timestamps.iter())
            .find(|(instant, _)| {
                now.checked_duration_since(**instant)
                    .map(|elapsed| elapsed <= window)
                    .unwrap_or(false)
            })
            .map(|(_, timestamp)| *timestamp)
    }

    pub fn has_recent_file_writes(&self) -> bool {
        let now = Instant::now();
        let window = Duration::from_secs(TOOL_LOOP_WINDOW_SECS);

        self.recent_file_write_times.values().any(|times| {
            times.iter().any(|timestamp| {
                now.checked_duration_since(*timestamp)
                    .map(|elapsed| elapsed <= window)
                    .unwrap_or(false)
            })
        })
    }

    fn apply_test_result(
        &mut self,
        tool_name: Option<&String>,
        is_sidechain: bool,
        is_error: bool,
        output: Option<&String>,
        stderr: Option<&String>,
    ) {
        let Some(index) = self.recent_tool_calls.iter().rposition(|call| {
            call.is_sidechain == is_sidechain
                && tool_name
                    .map(|tool_name| tool_name == &call.tool_name)
                    .unwrap_or(true)
        }) else {
            return;
        };

        let (tool_name, command) = {
            let last_call = &mut self.recent_tool_calls[index];
            last_call.is_error = is_error;
            (last_call.tool_name.clone(), last_call.command.clone())
        };

        if is_sidechain || !tool_name.eq_ignore_ascii_case("bash") {
            return;
        }

        let Some(command) = command else {
            return;
        };

        if !Self::is_test_command(&command) {
            self.reset_test_loop_tracking(true);
            return;
        }

        let failed = is_error
            || stderr.is_some_and(|stderr| !stderr.trim().is_empty())
            || output.is_some_and(|output| contains_test_failure_pattern(output));

        self.last_test_command = Some(command);
        if self.last_test_file.is_none() {
            self.last_test_file = self.last_file_path.clone();
        }

        if failed {
            self.consecutive_test_failures = self.consecutive_test_failures.saturating_add(1);
        } else {
            self.reset_test_loop_tracking(false);
        }
    }

    pub fn activity_score(&self, now: DateTime<Utc>, window: Duration) -> f32 {
        fn is_recent(
            timestamp: Option<DateTime<Utc>>,
            now: DateTime<Utc>,
            window: Duration,
        ) -> bool {
            timestamp.is_some_and(|timestamp| {
                now.signed_duration_since(timestamp)
                    .to_std()
                    .map(|elapsed| elapsed <= window)
                    .unwrap_or(true)
            })
        }

        let mut score = 0.0;

        if is_recent(self.last_main_jsonl_record_at, now, window) {
            score += 1.0;
        }
        if is_recent(self.last_sidechain_jsonl_record_at, now, window) {
            score += SIDECHAIN_ACTIVITY_WEIGHT;
        }
        if is_recent(self.last_file_event_at, now, window) {
            score += 1.0;
        }
        if is_recent(self.last_hook_event_at, now, window) {
            score += 1.0;
        }

        score
    }

    /// Applies an event to the tracked state.
    ///
    /// The update rules intentionally distinguish between activity that should reset
    /// silence tracking and passive monitoring signals. In particular,
    /// `ProcessMetrics` updates resource information without advancing
    /// `last_event_at`.
    pub fn apply(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::SessionStart {
                timestamp,
                session_id,
            } => {
                self.session_id = Some(session_id.clone());
                self.session_started_at = Some(*timestamp);
                self.last_jsonl_record_at = Some(*timestamp);
                self.last_main_jsonl_record_at = Some(*timestamp);
                self.last_event_at = Some(*timestamp);
                self.last_event_was_compact = false;
                self.awaiting_tool_result = false;
                self.subagent_count = 0;
                self.reset_test_loop_tracking(true);
            }
            AgentEvent::UserMessage {
                timestamp,
                is_sidechain,
            } => {
                self.mark_jsonl_activity(*timestamp, *is_sidechain);
            }
            AgentEvent::ToolCall(call) => {
                self.mark_jsonl_activity(call.timestamp, call.is_sidechain);
                self.last_tool_call_at = Some(call.timestamp);
                self.last_tool_name = Some(call.tool_name.clone());
                let previous_file_path = self.last_file_path.clone();
                self.last_file_path = call.file_path.clone();
                self.last_command = call.command.clone();
                self.total_tool_calls = self.total_tool_calls.saturating_add(1);
                if !call.is_sidechain {
                    self.last_event_was_compact = false;
                    self.awaiting_tool_result = true;

                    if call.is_write {
                        let changed_file = call.file_path.as_ref();
                        let should_reset =
                            changed_file.is_some() && self.last_test_file.as_ref() != changed_file;
                        if should_reset {
                            self.reset_test_loop_tracking(true);
                        }
                    } else if call
                        .command
                        .as_deref()
                        .is_some_and(|command| !Self::is_test_command(command))
                    {
                        self.reset_test_loop_tracking(true);
                    } else if call.command.as_deref().is_some_and(Self::is_test_command) {
                        self.last_test_file =
                            previous_file_path.or_else(|| self.last_test_file.clone());
                    }
                }

                if let Some(path) = call.file_path.clone() {
                    if let Some(kind) = tool_call_file_change_kind(call) {
                        if call.is_write {
                            self.track_touched_file(path.clone());
                        }
                        push_bounded(
                            &mut self.recently_modified_files,
                            FileModification {
                                timestamp: call.timestamp,
                                path: path.clone(),
                                kind,
                                line_change: call.line_change.clone(),
                            },
                            MAX_RECENTLY_MODIFIED_FILES,
                        );
                    }

                    if call.is_write
                        && !self.guard_paths.is_empty()
                        && !path_matches_any_guard(&path, &self.guard_paths)
                    {
                        self.scope_violation_count = self.scope_violation_count.saturating_add(1);
                        self.latest_scope_violation = Some(path);
                    }
                }

                push_bounded(
                    &mut self.recent_tool_calls,
                    call.clone(),
                    MAX_RECENT_TOOL_CALLS,
                );
            }
            AgentEvent::ToolResult {
                timestamp,
                tool_name,
                is_error,
                output,
                stderr,
                is_sidechain,
                ..
            } => {
                self.mark_jsonl_activity(*timestamp, *is_sidechain);
                self.last_tool_result_at = Some(*timestamp);
                if let Some(tool_name) = tool_name.as_ref() {
                    self.last_tool_name = Some(tool_name.clone());
                }
                if !is_sidechain {
                    self.awaiting_tool_result = false;
                }

                self.apply_test_result(
                    tool_name.as_ref(),
                    *is_sidechain,
                    *is_error,
                    output.as_ref(),
                    stderr.as_ref(),
                );
            }
            AgentEvent::TokenActivity(activity) => {
                self.mark_jsonl_activity(activity.timestamp, activity.is_sidechain);
                self.last_token_activity_at = Some(activity.timestamp);
                self.total_input_tokens = self
                    .total_input_tokens
                    .saturating_add(activity.input_tokens);
                self.total_output_tokens = self
                    .total_output_tokens
                    .saturating_add(activity.output_tokens);
            }
            AgentEvent::FileModified(file_event) => {
                self.last_event_at = Some(file_event.timestamp);
                self.last_file_event_at = Some(file_event.timestamp);
                self.last_file_path = Some(file_event.path.clone());
                self.track_touched_file(file_event.path.clone());
                if matches!(
                    file_event.kind,
                    FileChangeKind::Created | FileChangeKind::Modified
                ) {
                    self.note_file_write(&file_event.path, file_event.timestamp);
                }
                push_bounded(
                    &mut self.recently_modified_files,
                    file_event.clone(),
                    MAX_RECENTLY_MODIFIED_FILES,
                );

                if !self.guard_paths.is_empty()
                    && !path_matches_any_guard(&file_event.path, &self.guard_paths)
                {
                    self.scope_violation_count = self.scope_violation_count.saturating_add(1);
                    self.latest_scope_violation = Some(file_event.path.clone());
                }
            }
            AgentEvent::ProcessMetrics(metrics) => {
                self.last_process_metrics_at = Some(metrics.timestamp);
                self.process_alive = metrics.process_alive;
                if self.latest_rss_bytes != metrics.rss_bytes
                    || self.latest_virtual_bytes != metrics.virtual_bytes
                {
                    self.last_memory_activity_at = Some(metrics.timestamp);
                }
                self.latest_cpu_percent = metrics.cpu_percent;
                self.latest_rss_bytes = metrics.rss_bytes;
                self.latest_virtual_bytes = metrics.virtual_bytes;
                self.latest_io_read_bytes = metrics.io_read_bytes;
                self.latest_io_write_bytes = metrics.io_write_bytes;
                self.latest_io_read_rate = metrics.io_read_rate;
                self.latest_io_write_rate = metrics.io_write_rate;
                if metrics.io_read_rate > 0.0 || metrics.io_write_rate > 0.0 {
                    self.last_io_activity_at = Some(metrics.timestamp);
                }
            }
            AgentEvent::CompactBoundary {
                timestamp,
                is_sidechain,
            } => {
                self.mark_jsonl_activity(*timestamp, *is_sidechain);
                self.last_compact_at = Some(*timestamp);
                if !is_sidechain {
                    self.compact_count = self.compact_count.saturating_add(1);
                    self.last_event_was_compact = true;
                    self.awaiting_tool_result = false;
                    self.reset_test_loop_tracking(true);
                }
            }
        }
    }
}

fn tool_call_file_change_kind(call: &ToolCall) -> Option<FileChangeKind> {
    let tool_name = call.tool_name.to_ascii_lowercase();
    match tool_name.as_str() {
        "read" => Some(FileChangeKind::Read),
        "edit" | "multiedit" | "notebookedit" => Some(FileChangeKind::Modified),
        "write" => Some(
            if call
                .line_change
                .as_ref()
                .is_some_and(|change| change.removed == 0)
            {
                FileChangeKind::Created
            } else {
                FileChangeKind::Modified
            },
        ),
        _ => None,
    }
}

fn push_bounded<T>(deque: &mut VecDeque<T>, value: T, max_len: usize) {
    deque.push_back(value);
    while deque.len() > max_len {
        deque.pop_front();
    }
}

fn contains_sequence(tokens: &[String], sequence: &[&str]) -> bool {
    tokens.windows(sequence.len()).any(|window| {
        window
            .iter()
            .map(String::as_str)
            .eq(sequence.iter().copied())
    })
}

fn contains_test_failure_pattern(content: &str) -> bool {
    ["FAILED", "error[E", "panicked", "ERRORS", "npm ERR!"]
        .into_iter()
        .any(|pattern| content.contains(pattern))
}

#[cfg(test)]
mod tests {
    use super::{AgentEvent, AgentState, ProcessMetrics, ToolCall};
    use chrono::{TimeZone, Utc};
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::time::Duration;

    fn ts(hour: u32, minute: u32, second: u32) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 24, hour, minute, second)
            .single()
            .expect("valid timestamp")
    }

    #[test]
    fn activity_score_weights_sidechain_activity() {
        let now = ts(12, 0, 0);
        let state = AgentState {
            last_main_jsonl_record_at: Some(ts(11, 59, 50)),
            last_sidechain_jsonl_record_at: Some(ts(11, 59, 55)),
            ..Default::default()
        };

        assert!((state.activity_score(now, Duration::from_secs(30)) - 1.3).abs() < 0.001);
    }

    #[test]
    fn sidechain_tool_call_tracks_subagent_without_arming_api_wait() {
        let session_start = ts(12, 0, 0);
        let sidechain_call_at = ts(12, 0, 10);
        let mut state = AgentState::default();

        state.apply(&AgentEvent::SessionStart {
            timestamp: session_start,
            session_id: "ses-1".into(),
        });
        state.apply(&AgentEvent::ToolCall(ToolCall {
            timestamp: sidechain_call_at,
            tool_name: "Bash".into(),
            file_path: Some(PathBuf::from("/tmp/subagent.txt")),
            command: Some("cargo test".into()),
            line_change: None,
            is_error: false,
            is_write: false,
            is_sidechain: true,
        }));

        assert_eq!(state.last_main_jsonl_record_at, Some(session_start));
        assert_eq!(
            state.last_sidechain_jsonl_record_at,
            Some(sidechain_call_at)
        );
        assert_eq!(state.last_jsonl_record_at, Some(sidechain_call_at));
        assert_eq!(state.subagent_count, 1);
        assert!(!state.awaiting_tool_result);
    }

    #[test]
    fn sidechain_tool_result_does_not_clear_main_tool_wait() {
        let session_start = ts(12, 0, 0);
        let main_call_at = ts(12, 0, 5);
        let sidechain_result_at = ts(12, 0, 10);
        let mut state = AgentState::default();

        state.apply(&AgentEvent::SessionStart {
            timestamp: session_start,
            session_id: "ses-1".into(),
        });
        state.apply(&AgentEvent::ToolCall(ToolCall {
            timestamp: main_call_at,
            tool_name: "Bash".into(),
            file_path: None,
            command: Some("cargo test".into()),
            line_change: None,
            is_error: false,
            is_write: false,
            is_sidechain: false,
        }));
        state.apply(&AgentEvent::ToolResult {
            timestamp: sidechain_result_at,
            tool_name: Some("Bash".into()),
            is_error: true,
            output: Some("boom".into()),
            stderr: Some("boom".into()),
            interrupted: false,
            persisted_output_path: None,
            is_sidechain: true,
        });

        assert!(state.awaiting_tool_result);
        assert_eq!(state.subagent_count, 1);
        assert_eq!(state.last_main_jsonl_record_at, Some(main_call_at));
        assert_eq!(
            state.last_sidechain_jsonl_record_at,
            Some(sidechain_result_at)
        );
        assert!(
            !state
                .recent_tool_calls
                .back()
                .expect("main call exists")
                .is_error
        );
    }

    #[test]
    fn process_metrics_track_last_io_activity_only_when_rate_is_non_zero() {
        let mut state = AgentState::default();
        let idle_ts = ts(12, 0, 0);
        state.apply(&AgentEvent::ProcessMetrics(ProcessMetrics {
            timestamp: idle_ts,
            process_alive: true,
            cpu_percent: 0.0,
            rss_bytes: 10,
            virtual_bytes: 20,
            io_read_bytes: 100,
            io_write_bytes: 200,
            io_read_rate: 0.0,
            io_write_rate: 0.0,
        }));

        assert_eq!(state.last_io_activity_at, None);
        assert_eq!(state.last_memory_activity_at, Some(idle_ts));
        assert_eq!(state.latest_io_read_rate, 0.0);
        assert_eq!(state.latest_io_write_rate, 0.0);

        let active_ts = ts(12, 0, 5);
        state.apply(&AgentEvent::ProcessMetrics(ProcessMetrics {
            timestamp: active_ts,
            process_alive: true,
            cpu_percent: 0.0,
            rss_bytes: 10,
            virtual_bytes: 20,
            io_read_bytes: 300,
            io_write_bytes: 500,
            io_read_rate: 40.0,
            io_write_rate: 60.0,
        }));

        assert_eq!(state.last_io_activity_at, Some(active_ts));
        assert_eq!(state.last_memory_activity_at, Some(idle_ts));
        assert_eq!(state.latest_io_read_rate, 40.0);
        assert_eq!(state.latest_io_write_rate, 60.0);
    }

    #[test]
    fn tracks_consecutive_test_failures_from_tool_results() {
        let mut state = AgentState {
            last_file_path: Some(PathBuf::from("src/lib.rs")),
            recent_tool_calls: VecDeque::from(vec![ToolCall {
                timestamp: ts(12, 0, 1),
                tool_name: "Bash".into(),
                file_path: None,
                command: Some("cargo test".into()),
                line_change: None,
                is_error: false,
                is_write: false,
                is_sidechain: false,
            }]),
            ..Default::default()
        };

        state.apply(&AgentEvent::ToolResult {
            timestamp: ts(12, 0, 2),
            tool_name: Some("Bash".into()),
            is_error: true,
            output: Some("FAILED tests::it_works".into()),
            stderr: None,
            interrupted: false,
            persisted_output_path: None,
            is_sidechain: false,
        });

        assert_eq!(state.consecutive_test_failures, 1);
        assert_eq!(state.last_test_command.as_deref(), Some("cargo test"));
        assert_eq!(state.last_test_file, Some(PathBuf::from("src/lib.rs")));
    }

    #[test]
    fn resets_test_failures_on_non_test_command_and_success() {
        let mut state = AgentState {
            consecutive_test_failures: 2,
            last_test_command: Some("cargo test".into()),
            last_test_file: Some(PathBuf::from("src/lib.rs")),
            last_file_path: Some(PathBuf::from("src/lib.rs")),
            ..Default::default()
        };

        state.apply(&AgentEvent::ToolCall(ToolCall {
            timestamp: ts(12, 0, 1),
            tool_name: "Bash".into(),
            file_path: None,
            command: Some("cargo fmt --check".into()),
            line_change: None,
            is_error: false,
            is_write: false,
            is_sidechain: false,
        }));

        assert_eq!(state.consecutive_test_failures, 0);
        assert_eq!(state.last_test_command, None);
        assert_eq!(state.last_test_file, None);

        state.apply(&AgentEvent::FileModified(super::FileModification {
            timestamp: ts(12, 0, 2),
            path: PathBuf::from("src/lib.rs"),
            kind: super::FileChangeKind::Modified,
            line_change: None,
        }));
        state.apply(&AgentEvent::ToolCall(ToolCall {
            timestamp: ts(12, 0, 2),
            tool_name: "Bash".into(),
            file_path: None,
            command: Some("cargo test".into()),
            line_change: None,
            is_error: false,
            is_write: false,
            is_sidechain: false,
        }));
        state.apply(&AgentEvent::ToolResult {
            timestamp: ts(12, 0, 3),
            tool_name: Some("Bash".into()),
            is_error: false,
            output: Some("ok".into()),
            stderr: None,
            interrupted: false,
            persisted_output_path: None,
            is_sidechain: false,
        });

        assert_eq!(state.consecutive_test_failures, 0);
        assert_eq!(state.last_test_command.as_deref(), Some("cargo test"));
        assert_eq!(state.last_test_file, Some(PathBuf::from("src/lib.rs")));
    }

    #[test]
    fn resets_test_failures_when_editing_different_file() {
        let mut state = AgentState {
            consecutive_test_failures: 2,
            last_test_command: Some("cargo test".into()),
            last_test_file: Some(PathBuf::from("src/lib.rs")),
            ..Default::default()
        };

        state.apply(&AgentEvent::ToolCall(ToolCall {
            timestamp: ts(12, 0, 1),
            tool_name: "Edit".into(),
            file_path: Some(PathBuf::from("src/other.rs")),
            command: None,
            line_change: None,
            is_error: false,
            is_write: true,
            is_sidechain: false,
        }));

        assert_eq!(state.consecutive_test_failures, 0);
        assert_eq!(state.last_test_command, None);
        assert_eq!(state.last_test_file, None);
    }

    #[test]
    fn file_modified_events_track_and_reset_tool_loop_state() {
        let mut state = AgentState::default();

        state.apply(&AgentEvent::FileModified(super::FileModification {
            timestamp: ts(12, 0, 0),
            path: PathBuf::from("src/lib.rs"),
            kind: super::FileChangeKind::Modified,
            line_change: None,
        }));
        state.apply(&AgentEvent::FileModified(super::FileModification {
            timestamp: ts(12, 0, 1),
            path: PathBuf::from("src/lib.rs"),
            kind: super::FileChangeKind::Modified,
            line_change: None,
        }));
        state.apply(&AgentEvent::FileModified(super::FileModification {
            timestamp: ts(12, 0, 2),
            path: PathBuf::from("src/lib.rs"),
            kind: super::FileChangeKind::Modified,
            line_change: None,
        }));

        assert_eq!(
            state.recent_file_write_count(&PathBuf::from("src/lib.rs")),
            3
        );
        assert_eq!(
            state.recent_file_write_started_at(&PathBuf::from("src/lib.rs")),
            Some(ts(12, 0, 0))
        );
        assert_eq!(state.last_loop_file_path, Some(PathBuf::from("src/lib.rs")));
        assert_eq!(state.loop_started_at, Some(ts(12, 0, 0)));

        state.apply(&AgentEvent::FileModified(super::FileModification {
            timestamp: ts(12, 0, 3),
            path: PathBuf::from("src/other.rs"),
            kind: super::FileChangeKind::Modified,
            line_change: None,
        }));

        assert_eq!(
            state.recent_file_write_count(&PathBuf::from("src/lib.rs")),
            0
        );
        assert_eq!(
            state.recent_file_write_count(&PathBuf::from("src/other.rs")),
            1
        );
        assert_eq!(
            state.last_loop_file_path,
            Some(PathBuf::from("src/other.rs"))
        );
        assert_eq!(state.loop_started_at, None);
    }
}
