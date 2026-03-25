pub mod classifier;
pub mod loop_detector;
pub mod metrics;
pub mod session;
pub mod types;

pub use classifier::{classify, ClassifierConfig};
pub use loop_detector::compute_loop_score;
pub use metrics::{
    activity_weight, compute_io_rate, compute_silence, has_recent_io_activity, is_bash_failure,
    JsonlRecord, TokenDelta, TokenTracker, ToolUseResult,
};
pub use session::SessionRecord;
pub use types::{
    AgentEvent, AgentPhase, AgentState, FileChangeKind, FileModification, LoopScore,
    ProcessMetrics, TaskFile, TokenActivity, ToolCall, ToolLineChange,
};

#[cfg(test)]
mod classifier_tests {
    use super::{
        classify, AgentEvent, AgentPhase, AgentState, ClassifierConfig, FileChangeKind,
        FileModification, ToolCall,
    };
    use chrono::{Duration as ChronoDuration, Utc};
    use std::collections::VecDeque;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    #[test]
    fn classifies_thinking_and_api_hang() {
        let now = Utc::now();
        let thinking_state = AgentState {
            last_event_at: Some(now - ChronoDuration::seconds(50)),
            ..Default::default()
        };

        let thinking = classify(
            &thinking_state,
            now,
            ClassifierConfig {
                thinking_after: Duration::from_secs(45),
                api_hang_after: Duration::from_secs(130),
                ..ClassifierConfig::default()
            },
        );
        assert_eq!(thinking, AgentPhase::Thinking);

        let api_hang_state = AgentState {
            last_event_at: Some(now - ChronoDuration::seconds(131)),
            last_jsonl_record_at: Some(now - ChronoDuration::seconds(131)),
            awaiting_tool_result: true,
            latest_cpu_percent: 0.5,
            last_token_activity_at: Some(now - ChronoDuration::seconds(131)),
            ..Default::default()
        };
        let api_hang = classify(
            &api_hang_state,
            now,
            ClassifierConfig {
                thinking_after: Duration::from_secs(45),
                api_hang_after: Duration::from_secs(130),
                ..ClassifierConfig::default()
            },
        );
        assert!(matches!(api_hang, AgentPhase::ApiHang(_)));
    }

    #[test]
    fn keeps_long_user_silence_as_thinking() {
        let now = Utc::now();
        let state = AgentState {
            last_event_at: Some(now - ChronoDuration::seconds(131)),
            ..Default::default()
        };

        let phase = classify(
            &state,
            now,
            ClassifierConfig {
                thinking_after: Duration::from_secs(45),
                api_hang_after: Duration::from_secs(130),
                ..ClassifierConfig::default()
            },
        );

        assert_eq!(phase, AgentPhase::Thinking);
    }

    #[test]
    fn keeps_api_wait_with_recent_io_as_thinking() {
        let now = Utc::now();
        let state = AgentState {
            last_event_at: Some(now - ChronoDuration::seconds(131)),
            last_jsonl_record_at: Some(now - ChronoDuration::seconds(131)),
            awaiting_tool_result: true,
            latest_cpu_percent: 0.0,
            last_token_activity_at: Some(now - ChronoDuration::seconds(131)),
            last_io_activity_at: Some(now - ChronoDuration::seconds(30)),
            ..Default::default()
        };

        let phase = classify(
            &state,
            now,
            ClassifierConfig {
                thinking_after: Duration::from_secs(45),
                api_hang_after: Duration::from_secs(130),
                io_inactive_after: Duration::from_secs(60),
                ..ClassifierConfig::default()
            },
        );

        assert_eq!(phase, AgentPhase::Thinking);
    }

    #[test]
    fn sidechain_silence_alone_does_not_trigger_api_hang() {
        let now = Utc::now();
        let state = AgentState {
            session_started_at: Some(now - ChronoDuration::seconds(200)),
            last_main_jsonl_record_at: Some(now - ChronoDuration::seconds(200)),
            last_sidechain_jsonl_record_at: Some(now - ChronoDuration::seconds(131)),
            last_jsonl_record_at: Some(now - ChronoDuration::seconds(131)),
            last_event_at: Some(now - ChronoDuration::seconds(131)),
            ..Default::default()
        };

        let phase = classify(
            &state,
            now,
            ClassifierConfig {
                thinking_after: Duration::from_secs(45),
                api_hang_after: Duration::from_secs(130),
                ..ClassifierConfig::default()
            },
        );

        assert_eq!(phase, AgentPhase::Thinking);
    }

    #[test]
    fn classifies_tool_loop_test_loop_and_scope_leak() {
        let now = Utc::now();

        let mut tool_loop_state = AgentState {
            last_event_at: Some(now),
            recent_tool_calls: VecDeque::from(vec![
                ToolCall {
                    timestamp: now - ChronoDuration::minutes(2),
                    tool_name: "Write".into(),
                    file_path: Some(PathBuf::from("src/lib.rs")),
                    command: None,
                    line_change: None,
                    is_error: false,
                    is_write: true,
                    is_sidechain: false,
                },
                ToolCall {
                    timestamp: now - ChronoDuration::minutes(1),
                    tool_name: "Edit".into(),
                    file_path: Some(PathBuf::from("src/lib.rs")),
                    command: None,
                    line_change: None,
                    is_error: false,
                    is_write: true,
                    is_sidechain: false,
                },
                ToolCall {
                    timestamp: now,
                    tool_name: "Write".into(),
                    file_path: Some(PathBuf::from("src/lib.rs")),
                    command: None,
                    line_change: None,
                    is_error: false,
                    is_write: true,
                    is_sidechain: false,
                },
            ]),
            ..Default::default()
        };
        tool_loop_state.apply(&AgentEvent::FileModified(FileModification {
            timestamp: now - ChronoDuration::minutes(2),
            path: PathBuf::from("src/lib.rs"),
            kind: FileChangeKind::Modified,
            line_change: None,
        }));
        tool_loop_state.apply(&AgentEvent::FileModified(FileModification {
            timestamp: now - ChronoDuration::minutes(1),
            path: PathBuf::from("src/lib.rs"),
            kind: FileChangeKind::Modified,
            line_change: None,
        }));
        tool_loop_state.apply(&AgentEvent::FileModified(FileModification {
            timestamp: now,
            path: PathBuf::from("src/lib.rs"),
            kind: FileChangeKind::Modified,
            line_change: None,
        }));
        assert!(matches!(
            classify(&tool_loop_state, now, ClassifierConfig::default()),
            AgentPhase::ToolLoop { count: 3, .. }
        ));

        let test_loop_state = AgentState {
            last_event_at: Some(now),
            recent_tool_calls: VecDeque::from(vec![
                ToolCall {
                    timestamp: now - ChronoDuration::minutes(2),
                    tool_name: "Bash".into(),
                    file_path: None,
                    command: Some("cargo test".into()),
                    line_change: None,
                    is_error: true,
                    is_write: false,
                    is_sidechain: false,
                },
                ToolCall {
                    timestamp: now - ChronoDuration::minutes(1),
                    tool_name: "Bash".into(),
                    file_path: None,
                    command: Some("cargo test -p saw-core".into()),
                    line_change: None,
                    is_error: true,
                    is_write: false,
                    is_sidechain: false,
                },
                ToolCall {
                    timestamp: now,
                    tool_name: "Bash".into(),
                    file_path: None,
                    command: Some("pytest tests/unit".into()),
                    line_change: None,
                    is_error: true,
                    is_write: false,
                    is_sidechain: false,
                },
            ]),
            consecutive_test_failures: 3,
            last_test_command: Some("pytest tests/unit".into()),
            ..Default::default()
        };
        assert!(matches!(
            classify(&test_loop_state, now, ClassifierConfig::default()),
            AgentPhase::TestLoop {
                failure_count: 3,
                command,
            } if command == "pytest tests/unit"
        ));

        let scope_leak_state = AgentState {
            last_event_at: Some(now),
            guard_paths: vec![PathBuf::from("/repo/src/auth")],
            latest_scope_violation: Some(PathBuf::from("/repo/src/billing/mod.rs")),
            ..Default::default()
        };
        assert!(matches!(
            classify(&scope_leak_state, now, ClassifierConfig::default()),
            AgentPhase::ScopeLeaking { .. }
        ));
    }

    #[test]
    fn does_not_classify_writes_inside_guard_as_scope_leaks() {
        let now = Utc::now();
        let mut state = AgentState {
            last_event_at: Some(now),
            guard_paths: vec![PathBuf::from("/repo/src/auth")],
            ..Default::default()
        };
        state.apply(&AgentEvent::FileModified(FileModification {
            timestamp: now,
            path: PathBuf::from("/repo/src/auth/login.rs"),
            kind: FileChangeKind::Modified,
            line_change: None,
        }));

        assert_eq!(
            classify(&state, now, ClassifierConfig::default()),
            AgentPhase::Working
        );
        assert_eq!(state.latest_scope_violation, None);
    }

    #[test]
    fn classifies_scope_leak_against_most_specific_guard_when_multiple_guards_exist() {
        let now = Utc::now();
        let mut state = AgentState {
            last_event_at: Some(now),
            guard_paths: vec![
                PathBuf::from("/repo/tests/auth"),
                PathBuf::from("/repo/src/auth"),
            ],
            ..Default::default()
        };
        state.apply(&AgentEvent::FileModified(FileModification {
            timestamp: now,
            path: PathBuf::from("/repo/src/billing/mod.rs"),
            kind: FileChangeKind::Modified,
            line_change: None,
        }));

        assert!(matches!(
            classify(&state, now, ClassifierConfig::default()),
            AgentPhase::ScopeLeaking {
                violating_file,
                guard_path,
            } if violating_file.as_path() == Path::new("/repo/src/billing/mod.rs")
                && guard_path.as_path() == Path::new("/repo/src/auth")
        ));
    }
}
