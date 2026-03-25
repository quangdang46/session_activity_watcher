use crate::loop_detector::compute_loop_score;
use crate::metrics::{compute_silence, has_recent_io_activity};
use crate::types::{path_matches_any_guard, AgentPhase, AgentState};
use chrono::{DateTime, Utc};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct ClassifierConfig {
    pub thinking_after: Duration,
    pub api_hang_after: Duration,
    pub dead_after: Duration,
    pub idle_after: Duration,
    pub io_inactive_after: Duration,
    pub tool_loop_rewrites: u32,
    pub test_loop_failures: u32,
}

impl Default for ClassifierConfig {
    fn default() -> Self {
        Self {
            thinking_after: Duration::from_secs(30),
            api_hang_after: Duration::from_secs(120),
            dead_after: Duration::from_secs(300),
            idle_after: Duration::from_secs(600),
            io_inactive_after: Duration::from_secs(60),
            tool_loop_rewrites: 3,
            test_loop_failures: 3,
        }
    }
}

pub fn classify(state: &AgentState, now: DateTime<Utc>, cfg: ClassifierConfig) -> AgentPhase {
    if is_dead(state, now, cfg) {
        return AgentPhase::Dead;
    }

    if let Some((violating_file, guard_path)) = detect_scope_leak(state) {
        return AgentPhase::ScopeLeaking {
            violating_file,
            guard_path,
        };
    }

    if state.last_event_was_compact {
        return AgentPhase::ContextReset;
    }

    let loop_score = compute_loop_score(&state.recent_tool_calls);

    let has_recent_file_writes = state.has_recent_file_writes();

    if has_recent_file_writes {
        if let Some(file) = state.last_loop_file_path.clone() {
            let count = state.recent_file_write_count(&file);
            if count >= cfg.tool_loop_rewrites {
                return AgentPhase::ToolLoop {
                    since: state
                        .recent_file_write_started_at(&file)
                        .or_else(|| find_loop_started_at(state, &file))
                        .or(state.last_event_at)
                        .unwrap_or(now),
                    file,
                    count,
                };
            }
        }
    } else if loop_score.file_rewrites >= cfg.tool_loop_rewrites {
        if let Some(file) = loop_score.most_written_file {
            return AgentPhase::ToolLoop {
                since: find_loop_started_at(state, &file)
                    .or(state.last_event_at)
                    .unwrap_or(now),
                file,
                count: loop_score.file_rewrites,
            };
        }
    }

    let consecutive_test_failures = state
        .consecutive_test_failures
        .max(loop_score.consecutive_test_failures);
    if consecutive_test_failures >= cfg.test_loop_failures {
        if let Some(command) = state.last_test_command.clone() {
            return AgentPhase::TestLoop {
                command,
                failure_count: consecutive_test_failures,
            };
        }
    }

    if let Some(task) = detect_task_blocked(state) {
        return AgentPhase::TaskBlocked {
            task_id: task.id.clone(),
            blocked_by: unresolved_blockers(task, state),
        };
    }

    let silence = compute_silence(state, now);
    let has_recent_io = has_recent_io_activity(state, now, cfg.io_inactive_after);

    if is_api_hang(state, now, silence, has_recent_io, cfg) {
        return AgentPhase::ApiHang(silence);
    }

    if silence > cfg.idle_after {
        return AgentPhase::Idle(silence);
    }

    if silence > cfg.thinking_after {
        return AgentPhase::Thinking;
    }

    AgentPhase::Working
}

fn is_dead(state: &AgentState, now: DateTime<Utc>, cfg: ClassifierConfig) -> bool {
    if !state.process_alive {
        return true;
    }

    let silence = compute_silence(state, now);
    silence > cfg.dead_after
        && state.latest_cpu_percent < 0.5
        && no_recent_jsonl_records(state, now, cfg.dead_after)
        && tokens_are_frozen(state, now, cfg.dead_after)
        && memory_is_flat(state, now, cfg.dead_after)
}

fn detect_task_blocked(state: &AgentState) -> Option<&crate::types::TaskFile> {
    let task = state.current_task.as_ref()?;
    let unresolved = unresolved_blockers(task, state);
    (!unresolved.is_empty()).then_some(task)
}

fn unresolved_blockers(task: &crate::types::TaskFile, state: &AgentState) -> Vec<String> {
    if task.status != "in_progress" || task.blocked_by.is_empty() {
        return Vec::new();
    }

    task.blocked_by
        .iter()
        .filter(|blocking_id| {
            state
                .task_files
                .get(*blocking_id)
                .map(|blocking_task| blocking_task.status != "completed")
                .unwrap_or(true)
        })
        .cloned()
        .collect()
}

fn is_api_hang(
    state: &AgentState,
    now: DateTime<Utc>,
    silence: Duration,
    has_recent_io: bool,
    cfg: ClassifierConfig,
) -> bool {
    state.awaiting_tool_result
        && silence > cfg.api_hang_after
        && state.latest_cpu_percent < 1.0
        && !has_recent_io
        && tokens_are_frozen(state, now, cfg.api_hang_after)
}

fn no_recent_jsonl_records(state: &AgentState, now: DateTime<Utc>, threshold: Duration) -> bool {
    let Some(last_jsonl_record_at) = state.last_jsonl_record_at else {
        return true;
    };

    now.signed_duration_since(last_jsonl_record_at)
        .to_std()
        .map(|elapsed| elapsed > threshold)
        .unwrap_or(false)
}

fn memory_is_flat(state: &AgentState, now: DateTime<Utc>, threshold: Duration) -> bool {
    let Some(last_memory_activity_at) = state.last_memory_activity_at else {
        return true;
    };

    now.signed_duration_since(last_memory_activity_at)
        .to_std()
        .map(|elapsed| elapsed > threshold)
        .unwrap_or(false)
}

fn tokens_are_frozen(state: &AgentState, now: DateTime<Utc>, threshold: Duration) -> bool {
    let Some(last_token_activity_at) = state.last_token_activity_at else {
        return state.awaiting_tool_result;
    };

    now.signed_duration_since(last_token_activity_at)
        .to_std()
        .map(|elapsed| elapsed > threshold)
        .unwrap_or(false)
}

fn detect_scope_leak(state: &AgentState) -> Option<(PathBuf, PathBuf)> {
    if let Some(violating_file) = state.latest_scope_violation.clone() {
        let guard_path = matching_guard_path(&violating_file, &state.guard_paths)
            .or_else(|| state.guard_paths.first().cloned())?;
        return Some((violating_file, guard_path));
    }

    state
        .recent_tool_calls
        .iter()
        .rev()
        .filter(|call| call.is_write)
        .filter_map(|call| call.file_path.clone())
        .find_map(|violating_file| {
            let guard_path = matching_guard_path(&violating_file, &state.guard_paths)?;
            Some((violating_file, guard_path))
        })
}

fn matching_guard_path(path: &Path, guard_paths: &[PathBuf]) -> Option<PathBuf> {
    if path_matches_any_guard(path, guard_paths) {
        return None;
    }

    guard_paths
        .iter()
        .max_by_key(|guard_path| {
            (
                shared_prefix_len(path, guard_path),
                guard_path.components().count(),
            )
        })
        .cloned()
}

fn shared_prefix_len(path: &Path, guard_path: &Path) -> usize {
    path.components()
        .zip(guard_path.components())
        .take_while(|(path_component, guard_component)| path_component == guard_component)
        .count()
}

fn find_loop_started_at(state: &AgentState, file: &PathBuf) -> Option<DateTime<Utc>> {
    state.loop_started_at.or_else(|| {
        state
            .recent_tool_calls
            .iter()
            .filter(|call| call.is_write)
            .filter(|call| call.file_path.as_ref() == Some(file))
            .map(|call| call.timestamp)
            .min()
    })
}

#[cfg(test)]
mod tests {
    use super::{classify, ClassifierConfig};
    use crate::types::{AgentPhase, AgentState, TaskFile, ToolCall};
    use chrono::{DateTime, Duration as ChronoDuration, TimeZone, Utc};
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::time::Duration;

    fn ts(seconds_after_start: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 24, 12, 0, 0)
            .single()
            .expect("valid timestamp")
            + ChronoDuration::seconds(seconds_after_start)
    }

    fn state_with_silence(now: DateTime<Utc>, silence_secs: i64) -> AgentState {
        let last_activity_at = now - ChronoDuration::seconds(silence_secs);
        AgentState {
            session_started_at: Some(last_activity_at),
            last_event_at: Some(last_activity_at),
            last_jsonl_record_at: Some(last_activity_at),
            ..AgentState::default()
        }
    }

    fn write_call(timestamp: DateTime<Utc>, path: &str) -> ToolCall {
        ToolCall {
            timestamp,
            tool_name: "Write".into(),
            file_path: Some(PathBuf::from(path)),
            command: None,
            line_change: None,
            is_error: false,
            is_write: true,
            is_sidechain: false,
        }
    }

    fn failed_test_call(timestamp: DateTime<Utc>, command: &str) -> ToolCall {
        ToolCall {
            timestamp,
            tool_name: "Bash".into(),
            file_path: None,
            command: Some(command.into()),
            line_change: None,
            is_error: true,
            is_write: false,
            is_sidechain: false,
        }
    }

    fn successful_test_call(timestamp: DateTime<Utc>, command: &str) -> ToolCall {
        ToolCall {
            timestamp,
            tool_name: "Bash".into(),
            file_path: None,
            command: Some(command.into()),
            line_change: None,
            is_error: false,
            is_write: false,
            is_sidechain: false,
        }
    }

    fn non_test_bash_call(timestamp: DateTime<Utc>, command: &str) -> ToolCall {
        ToolCall {
            timestamp,
            tool_name: "Bash".into(),
            file_path: None,
            command: Some(command.into()),
            line_change: None,
            is_error: false,
            is_write: false,
            is_sidechain: false,
        }
    }

    #[test]
    fn classifies_dead_before_anything_else() {
        let now = ts(0);
        let mut state = state_with_silence(now, 700);
        state.process_alive = false;
        state.latest_scope_violation = Some(PathBuf::from("/repo/src/billing/mod.rs"));
        state.guard_paths = vec![PathBuf::from("/repo/src/auth")];

        assert_eq!(
            classify(&state, now, ClassifierConfig::default()),
            AgentPhase::Dead
        );
    }

    #[test]
    fn classifies_scope_leak_before_context_reset() {
        let now = ts(0);
        let mut state = state_with_silence(now, 5);
        state.last_event_was_compact = true;
        state.guard_paths = vec![PathBuf::from("/repo/src/auth")];
        state.latest_scope_violation = Some(PathBuf::from("/repo/src/billing/mod.rs"));

        assert!(matches!(
            classify(&state, now, ClassifierConfig::default()),
            AgentPhase::ScopeLeaking { .. }
        ));
    }

    #[test]
    fn classifies_context_reset_before_loops() {
        let now = ts(0);
        let mut state = state_with_silence(now, 5);
        state.last_event_was_compact = true;
        state.recent_tool_calls = VecDeque::from(vec![
            write_call(ts(-240), "src/lib.rs"),
            write_call(ts(-120), "src/lib.rs"),
            write_call(ts(0), "src/lib.rs"),
        ]);

        assert_eq!(
            classify(&state, now, ClassifierConfig::default()),
            AgentPhase::ContextReset
        );
    }

    #[test]
    fn classifies_tool_loop_before_test_loop() {
        let now = ts(0);
        let mut state = state_with_silence(now, 1);
        state.recent_tool_calls = VecDeque::from(vec![
            write_call(ts(-240), "src/lib.rs"),
            write_call(ts(-120), "src/lib.rs"),
            write_call(ts(-60), "src/lib.rs"),
            failed_test_call(ts(-30), "cargo test"),
            failed_test_call(ts(-20), "cargo test -p saw-core"),
            failed_test_call(ts(-10), "pytest tests/unit"),
        ]);
        state.consecutive_test_failures = 3;
        state.last_test_command = Some("pytest tests/unit".into());
        state.last_test_file = Some(PathBuf::from("src/lib.rs"));
        state.apply(&crate::types::AgentEvent::FileModified(
            crate::types::FileModification {
                timestamp: ts(-240),
                path: PathBuf::from("src/lib.rs"),
                kind: crate::types::FileChangeKind::Modified,
                line_change: None,
            },
        ));
        state.apply(&crate::types::AgentEvent::FileModified(
            crate::types::FileModification {
                timestamp: ts(-120),
                path: PathBuf::from("src/lib.rs"),
                kind: crate::types::FileChangeKind::Modified,
                line_change: None,
            },
        ));
        state.apply(&crate::types::AgentEvent::FileModified(
            crate::types::FileModification {
                timestamp: ts(-60),
                path: PathBuf::from("src/lib.rs"),
                kind: crate::types::FileChangeKind::Modified,
                line_change: None,
            },
        ));

        assert!(matches!(
            classify(&state, now, ClassifierConfig::default()),
            AgentPhase::ToolLoop {
                ref file,
                count: 3,
                since,
            } if file == &PathBuf::from("src/lib.rs") && since == ts(-240)
        ));
    }

    #[test]
    fn classifies_test_loop_before_api_hang() {
        let now = ts(0);
        let mut state = state_with_silence(now, 300);
        state.awaiting_tool_result = true;
        state.latest_cpu_percent = 0.0;
        state.last_token_activity_at = Some(now - ChronoDuration::seconds(300));
        state.recent_tool_calls = VecDeque::from(vec![
            failed_test_call(ts(-40), "cargo test"),
            failed_test_call(ts(-30), "cargo test -p saw-core"),
            failed_test_call(ts(-20), "pytest tests/unit"),
        ]);
        state.consecutive_test_failures = 3;
        state.last_test_command = Some("pytest tests/unit".into());
        state.last_test_file = Some(PathBuf::from("src/lib.rs"));

        assert!(matches!(
            classify(&state, now, ClassifierConfig::default()),
            AgentPhase::TestLoop {
                failure_count: 3,
                command,
            } if command == "pytest tests/unit"
        ));
    }

    #[test]
    fn classifies_task_blocked_when_current_task_has_unresolved_dependencies() {
        let now = ts(0);
        let mut state = state_with_silence(now, 5);
        state.current_task = Some(TaskFile {
            id: "3".into(),
            status: "in_progress".into(),
            owner: None,
            blocked_by: vec!["2".into(), "4".into()],
        });
        state.task_files.insert(
            "2".into(),
            TaskFile {
                id: "2".into(),
                status: "completed".into(),
                owner: None,
                blocked_by: Vec::new(),
            },
        );
        state.task_files.insert(
            "4".into(),
            TaskFile {
                id: "4".into(),
                status: "pending".into(),
                owner: None,
                blocked_by: Vec::new(),
            },
        );

        assert!(matches!(
            classify(&state, now, ClassifierConfig::default()),
            AgentPhase::TaskBlocked { task_id, blocked_by }
                if task_id == "3" && blocked_by == vec!["4".to_string()]
        ));
    }

    #[test]
    fn skips_task_blocked_when_dependencies_are_completed() {
        let now = ts(0);
        let mut state = state_with_silence(now, 5);
        state.current_task = Some(TaskFile {
            id: "3".into(),
            status: "in_progress".into(),
            owner: None,
            blocked_by: vec!["2".into()],
        });
        state.task_files.insert(
            "2".into(),
            TaskFile {
                id: "2".into(),
                status: "completed".into(),
                owner: None,
                blocked_by: Vec::new(),
            },
        );

        assert_eq!(
            classify(&state, now, ClassifierConfig::default()),
            AgentPhase::Working
        );
    }

    #[test]
    fn skips_task_blocked_without_current_task_context() {
        let now = ts(0);
        let state = state_with_silence(now, 5);

        assert_eq!(
            classify(&state, now, ClassifierConfig::default()),
            AgentPhase::Working
        );
    }

    #[test]
    fn classifies_dead_after_five_minutes_of_complete_inactivity() {
        let now = ts(0);
        let mut state = state_with_silence(now, 301);
        state.latest_cpu_percent = 0.4;
        state.last_token_activity_at = Some(now - ChronoDuration::seconds(301));
        state.last_memory_activity_at = Some(now - ChronoDuration::seconds(301));

        assert_eq!(
            classify(&state, now, ClassifierConfig::default()),
            AgentPhase::Dead
        );
    }

    #[test]
    fn does_not_classify_dead_before_five_minutes() {
        let now = ts(0);
        let mut state = state_with_silence(now, 120);
        state.latest_cpu_percent = 0.0;
        state.last_token_activity_at = Some(now - ChronoDuration::seconds(120));
        state.last_memory_activity_at = Some(now - ChronoDuration::seconds(120));

        assert_eq!(
            classify(&state, now, ClassifierConfig::default()),
            AgentPhase::Thinking
        );
    }

    #[test]
    fn does_not_classify_dead_when_memory_is_still_changing() {
        let now = ts(0);
        let mut state = state_with_silence(now, 301);
        state.latest_cpu_percent = 0.4;
        state.last_token_activity_at = Some(now - ChronoDuration::seconds(301));
        state.last_memory_activity_at = Some(now - ChronoDuration::seconds(30));

        assert_eq!(
            classify(&state, now, ClassifierConfig::default()),
            AgentPhase::Thinking
        );
    }

    #[test]
    fn successful_test_resets_test_loop() {
        let now = ts(0);
        let mut state = state_with_silence(now, 1);
        state.recent_tool_calls = VecDeque::from(vec![
            failed_test_call(ts(-30), "cargo test"),
            failed_test_call(ts(-20), "cargo test -p saw-core"),
            successful_test_call(ts(-10), "cargo test -p saw-core"),
        ]);
        state.last_test_command = Some("cargo test -p saw-core".into());
        state.last_test_file = Some(PathBuf::from("src/lib.rs"));

        assert_eq!(
            classify(&state, now, ClassifierConfig::default()),
            AgentPhase::Working
        );
    }

    #[test]
    fn non_test_command_resets_test_loop() {
        let now = ts(0);
        let mut state = state_with_silence(now, 1);
        state.recent_tool_calls = VecDeque::from(vec![
            failed_test_call(ts(-30), "cargo test"),
            failed_test_call(ts(-20), "cargo test -p saw-core"),
            non_test_bash_call(ts(-10), "cargo fmt --check"),
        ]);
        state.last_test_command = Some("cargo test -p saw-core".into());
        state.last_test_file = Some(PathBuf::from("src/lib.rs"));

        assert_eq!(
            classify(&state, now, ClassifierConfig::default()),
            AgentPhase::Working
        );
    }

    #[test]
    fn classifies_api_hang_when_silence_cpu_and_tokens_indicate_stall() {
        let now = ts(0);
        let mut state = state_with_silence(now, 121);
        state.awaiting_tool_result = true;
        state.latest_cpu_percent = 0.5;
        state.last_token_activity_at = Some(now - ChronoDuration::seconds(121));
        state.last_memory_activity_at = Some(now - ChronoDuration::seconds(121));

        assert!(matches!(
            classify(&state, now, ClassifierConfig::default()),
            AgentPhase::ApiHang(duration) if duration == Duration::from_secs(121)
        ));
    }

    #[test]
    fn keeps_api_wait_as_thinking_when_recent_io_exists() {
        let now = ts(0);
        let mut state = state_with_silence(now, 121);
        state.awaiting_tool_result = true;
        state.latest_cpu_percent = 0.0;
        state.last_token_activity_at = Some(now - ChronoDuration::seconds(121));
        state.last_memory_activity_at = Some(now - ChronoDuration::seconds(121));
        state.last_io_activity_at = Some(now - ChronoDuration::seconds(30));

        assert_eq!(
            classify(&state, now, ClassifierConfig::default()),
            AgentPhase::Thinking
        );
    }

    #[test]
    fn classifies_idle_after_extended_silence() {
        let now = ts(0);
        let state = state_with_silence(now, 601);

        assert!(matches!(
            classify(&state, now, ClassifierConfig::default()),
            AgentPhase::Idle(duration) if duration == Duration::from_secs(601)
        ));
    }

    #[test]
    fn classifies_thinking_after_short_silence() {
        let now = ts(0);
        let state = state_with_silence(now, 31);

        assert_eq!(
            classify(&state, now, ClassifierConfig::default()),
            AgentPhase::Thinking
        );
    }

    #[test]
    fn classifies_working_for_recent_activity() {
        let now = ts(0);
        let state = state_with_silence(now, 5);

        assert_eq!(
            classify(&state, now, ClassifierConfig::default()),
            AgentPhase::Working
        );
    }

    #[test]
    fn classifies_working_for_empty_state_without_panicking() {
        let now = ts(0);
        let state = AgentState::default();

        assert_eq!(
            classify(&state, now, ClassifierConfig::default()),
            AgentPhase::Working
        );
    }

    #[test]
    fn keeps_busy_waiting_session_out_of_api_hang() {
        let now = ts(0);
        let mut state = state_with_silence(now, 121);
        state.awaiting_tool_result = true;
        state.latest_cpu_percent = 5.0;
        state.last_token_activity_at = Some(now - ChronoDuration::seconds(121));

        assert_eq!(
            classify(&state, now, ClassifierConfig::default()),
            AgentPhase::Thinking
        );
    }
}
