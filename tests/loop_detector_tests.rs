use chrono::{Duration, TimeZone, Utc};
use saw_core::{
    classify, compute_loop_score, AgentEvent, AgentPhase, AgentState, ClassifierConfig,
    FileChangeKind, FileModification, ToolCall,
};
use std::collections::VecDeque;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::Instant;

fn ts(offset_seconds: i64) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 24, 12, 0, 0)
        .single()
        .expect("valid timestamp")
        + Duration::seconds(offset_seconds)
}

fn call(
    timestamp: chrono::DateTime<Utc>,
    tool_name: &str,
    file_path: Option<PathBuf>,
    command: Option<&str>,
    is_error: bool,
) -> ToolCall {
    ToolCall {
        timestamp,
        tool_name: tool_name.to_string(),
        file_path,
        command: command.map(str::to_string),
        line_change: None,
        is_error,
        is_write: matches!(tool_name, "Edit" | "Write" | "NotebookEdit" | "MultiEdit"),
        is_sidechain: false,
    }
}

fn state_with_calls(calls: Vec<ToolCall>) -> AgentState {
    let mut state = AgentState {
        last_event_at: calls.iter().map(|call| call.timestamp).max(),
        ..Default::default()
    };
    for call in calls {
        let file_event = if call.is_write {
            call.file_path.clone().map(|path| FileModification {
                timestamp: call.timestamp,
                path,
                kind: FileChangeKind::Modified,
                line_change: call.line_change.clone(),
            })
        } else {
            None
        };

        state.apply(&AgentEvent::ToolCall(call));
        if let Some(file_event) = file_event {
            state.apply(&AgentEvent::FileModified(file_event));
        }
    }
    state
}

#[test]
fn file_rewrites_trigger_tool_loop_after_three_rewrites_in_five_minutes() {
    let file = PathBuf::from("src/lib.rs");
    let state = state_with_calls(vec![
        call(ts(0), "Edit", Some(file.clone()), None, false),
        call(ts(120), "Write", Some(file.clone()), None, false),
        call(ts(299), "Edit", Some(file.clone()), None, false),
    ]);

    let score = compute_loop_score(&state.recent_tool_calls);
    assert_eq!(score.file_rewrites, 3);
    assert_eq!(score.most_written_file, Some(file.clone()));

    let phase = classify(&state, ts(299), ClassifierConfig::default());
    assert!(matches!(
        phase,
        AgentPhase::ToolLoop {
            file: loop_file,
            count: 3,
            since,
        } if loop_file == file && since == ts(0)
    ));
}

#[test]
fn consecutive_failed_tests_trigger_test_loop() {
    let mut state = state_with_calls(vec![
        call(ts(0), "Bash", None, Some("cargo test"), true),
        call(ts(60), "Bash", None, Some("cargo test -p saw-core"), true),
        call(ts(120), "Bash", None, Some("pytest tests/unit"), true),
    ]);

    let score = compute_loop_score(&state.recent_tool_calls);
    assert_eq!(score.consecutive_test_failures, 3);

    state.consecutive_test_failures = 3;
    state.last_test_command = Some("pytest tests/unit".into());

    let phase = classify(&state, ts(120), ClassifierConfig::default());
    assert!(matches!(
        phase,
        AgentPhase::TestLoop {
            failure_count: 3,
            command,
        } if command == "pytest tests/unit"
    ));
}

#[test]
fn different_files_do_not_trigger_tool_loop() {
    let state = state_with_calls(vec![
        call(ts(0), "Edit", Some(PathBuf::from("src/a.rs")), None, false),
        call(
            ts(60),
            "Write",
            Some(PathBuf::from("src/b.rs")),
            None,
            false,
        ),
        call(
            ts(120),
            "Edit",
            Some(PathBuf::from("src/c.rs")),
            None,
            false,
        ),
    ]);

    let score = compute_loop_score(&state.recent_tool_calls);
    assert_eq!(score.file_rewrites, 1);

    let phase = classify(&state, ts(120), ClassifierConfig::default());
    assert_eq!(phase, AgentPhase::Working);
}

#[test]
fn mixed_commands_do_not_trigger_test_loop() {
    let mut state = state_with_calls(vec![
        call(ts(0), "Bash", None, Some("cargo test"), true),
        call(ts(60), "Bash", None, Some("cargo test"), false),
        call(ts(120), "Bash", None, Some("cargo fmt --check"), true),
        call(ts(180), "Bash", None, Some("pytest tests/unit"), true),
    ]);

    let score = compute_loop_score(&state.recent_tool_calls);
    assert_eq!(score.consecutive_test_failures, 1);

    state.consecutive_test_failures = 1;
    state.last_test_command = Some("pytest tests/unit".into());

    let phase = classify(&state, ts(180), ClassifierConfig::default());
    assert_eq!(phase, AgentPhase::Working);
}

#[test]
fn sliding_window_only_counts_last_five_minutes() {
    let file = PathBuf::from("src/lib.rs");
    let calls = VecDeque::from(vec![
        call(ts(0), "Edit", Some(file.clone()), None, false),
        call(ts(1), "Write", Some(file.clone()), None, false),
        call(ts(301), "Edit", Some(file.clone()), None, false),
    ]);

    let score = compute_loop_score(&calls);
    assert_eq!(score.file_rewrites, 2);
    assert_eq!(score.most_written_file, Some(file));
}

#[test]
fn different_file_write_resets_tool_loop_state() {
    let mut state = AgentState::default();
    state.apply(&AgentEvent::FileModified(FileModification {
        timestamp: ts(0),
        path: PathBuf::from("src/lib.rs"),
        kind: FileChangeKind::Modified,
        line_change: None,
    }));
    state.apply(&AgentEvent::FileModified(FileModification {
        timestamp: ts(60),
        path: PathBuf::from("src/lib.rs"),
        kind: FileChangeKind::Modified,
        line_change: None,
    }));
    state.apply(&AgentEvent::FileModified(FileModification {
        timestamp: ts(120),
        path: PathBuf::from("src/other.rs"),
        kind: FileChangeKind::Modified,
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
        classify(&state, ts(120), ClassifierConfig::default()),
        AgentPhase::Working
    );
}

#[test]
fn loop_score_average_time_stays_under_one_millisecond() {
    let calls = VecDeque::from(vec![
        call(
            ts(0),
            "Edit",
            Some(PathBuf::from("src/lib.rs")),
            None,
            false,
        ),
        call(
            ts(30),
            "Write",
            Some(PathBuf::from("src/lib.rs")),
            None,
            false,
        ),
        call(
            ts(60),
            "Edit",
            Some(PathBuf::from("src/lib.rs")),
            None,
            false,
        ),
        call(ts(90), "Bash", None, Some("cargo test"), true),
        call(ts(120), "Bash", None, Some("cargo test -p saw-core"), true),
        call(ts(150), "Bash", None, Some("pytest tests/unit"), true),
    ]);
    let iterations = 2_000u128;

    let start = Instant::now();
    for _ in 0..iterations {
        black_box(compute_loop_score(black_box(&calls)));
    }
    let elapsed = start.elapsed();
    let average_nanos = elapsed.as_nanos() / iterations;

    assert!(
        average_nanos < 1_000_000,
        "average compute_loop_score time was {average_nanos}ns"
    );
}

#[test]
fn recent_tool_history_is_capped_to_limit_loop_detector_input() {
    let mut state = AgentState::default();

    for index in 0..60 {
        state.apply(&AgentEvent::ToolCall(call(
            ts(index),
            "Edit",
            Some(PathBuf::from(format!("src/file-{index}.rs"))),
            None,
            false,
        )));
    }

    assert_eq!(state.recent_tool_calls.len(), 50);
    assert_eq!(
        state.recent_tool_calls.front().map(|call| call.timestamp),
        Some(ts(10))
    );

    let score = compute_loop_score(&state.recent_tool_calls);
    assert_eq!(score.file_rewrites, 1);
}

#[test]
fn success_and_different_file_reset_classifier_test_loop_state() {
    let mut success_state = state_with_calls(vec![
        call(ts(0), "Bash", None, Some("cargo test"), true),
        call(ts(60), "Bash", None, Some("cargo test -p saw-core"), true),
        call(ts(120), "Bash", None, Some("cargo test -p saw-core"), false),
    ]);
    success_state.last_test_command = Some("cargo test -p saw-core".into());
    success_state.last_test_file = Some(PathBuf::from("src/lib.rs"));

    assert_eq!(
        classify(&success_state, ts(120), ClassifierConfig::default()),
        AgentPhase::Working
    );

    let edited_file_state = AgentState {
        last_event_at: Some(ts(120)),
        consecutive_test_failures: 2,
        last_test_command: Some("cargo test -p saw-core".into()),
        last_test_file: Some(PathBuf::from("src/lib.rs")),
        recent_tool_calls: VecDeque::from(vec![call(
            ts(120),
            "Edit",
            Some(PathBuf::from("src/other.rs")),
            None,
            false,
        )]),
        ..Default::default()
    };

    assert_eq!(
        classify(&edited_file_state, ts(120), ClassifierConfig::default()),
        AgentPhase::Working
    );
}

#[test]
fn context_reset_persists_until_next_tool_call() {
    let mut state = AgentState::default();
    state.apply(&AgentEvent::CompactBoundary {
        timestamp: ts(0),
        is_sidechain: false,
    });

    assert_eq!(state.compact_count, 1);
    assert_eq!(
        classify(&state, ts(0), ClassifierConfig::default()),
        AgentPhase::ContextReset
    );

    state.apply(&AgentEvent::UserMessage {
        timestamp: ts(10),
        is_sidechain: false,
    });

    assert!(state.last_event_was_compact);
    assert_eq!(
        classify(&state, ts(10), ClassifierConfig::default()),
        AgentPhase::ContextReset
    );

    state.apply(&AgentEvent::ToolCall(call(
        ts(20),
        "Read",
        Some(PathBuf::from("src/lib.rs")),
        None,
        false,
    )));

    assert!(!state.last_event_was_compact);
    assert_eq!(
        classify(&state, ts(20), ClassifierConfig::default()),
        AgentPhase::Working
    );
}

#[test]
fn sidechain_compact_boundary_does_not_arm_context_reset() {
    let mut state = AgentState::default();
    state.apply(&AgentEvent::CompactBoundary {
        timestamp: ts(0),
        is_sidechain: true,
    });

    assert_eq!(state.compact_count, 0);
    assert!(!state.last_event_was_compact);
    assert_eq!(
        classify(&state, ts(0), ClassifierConfig::default()),
        AgentPhase::Working
    );
}
