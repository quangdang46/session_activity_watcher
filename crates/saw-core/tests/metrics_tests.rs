use chrono::{Duration as ChronoDuration, TimeZone, Utc};
use saw_core::{
    activity_weight, compute_io_rate, compute_silence, is_bash_failure, AgentState, JsonlRecord,
    TokenTracker,
};
use serde_json::json;
use std::time::{Duration, Instant};

fn ts(hour: u32, minute: u32, second: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 3, 24, hour, minute, second)
        .single()
        .expect("valid timestamp")
}

fn parse_record(value: serde_json::Value) -> JsonlRecord {
    serde_json::from_value(value).expect("valid JsonlRecord fixture")
}

#[test]
fn compute_silence_uses_most_recent_visible_activity_source() {
    let now = ts(12, 0, 0);
    let state = AgentState {
        session_started_at: Some(now - ChronoDuration::seconds(300)),
        last_event_at: Some(now - ChronoDuration::seconds(5)),
        last_jsonl_record_at: Some(now - ChronoDuration::seconds(40)),
        last_file_event_at: Some(now - ChronoDuration::seconds(18)),
        last_hook_event_at: Some(now - ChronoDuration::seconds(9)),
        ..Default::default()
    };

    assert_eq!(compute_silence(&state, now), Duration::from_secs(9));
}

#[test]
fn compute_silence_falls_back_when_visible_activity_is_missing() {
    let now = ts(12, 0, 0);

    let state = AgentState {
        last_event_at: Some(now - ChronoDuration::seconds(15)),
        ..Default::default()
    };
    assert_eq!(compute_silence(&state, now), Duration::from_secs(15));

    let state = AgentState {
        session_started_at: Some(now - ChronoDuration::seconds(45)),
        ..Default::default()
    };
    assert_eq!(compute_silence(&state, now), Duration::from_secs(45));
}

#[test]
fn compute_io_rate_uses_elapsed_seconds() {
    assert_eq!(compute_io_rate(100, 700, Duration::from_secs(3)), 200.0);
    assert_eq!(compute_io_rate(700, 100, Duration::from_secs(3)), 0.0);
    assert_eq!(compute_io_rate(100, 700, Duration::ZERO), 0.0);
}

#[test]
fn token_tracker_is_frozen_detects_recent_and_stalled_trackers() {
    let recent_tracker = TokenTracker::new(Instant::now());
    assert!(!recent_tracker.is_frozen(Duration::from_secs(1)));

    let stalled_tracker = TokenTracker::new(Instant::now() - Duration::from_secs(2));
    assert!(stalled_tracker.is_frozen(Duration::from_secs(1)));
}

#[test]
fn activity_weight_returns_expected_sidechain_scaling() {
    assert_eq!(activity_weight(false), 1.0);
    assert_eq!(activity_weight(true), 0.3);
}

#[test]
fn compute_silence_ignores_compact_boundary_until_next_tool_call() {
    let now = ts(12, 0, 30);
    let mut state = AgentState::default();
    state.apply(&saw_core::AgentEvent::CompactBoundary {
        timestamp: ts(12, 0, 0),
        is_sidechain: false,
    });

    assert_eq!(compute_silence(&state, now), Duration::from_secs(30));
    assert!(state.last_event_was_compact);
}

#[test]
fn is_bash_failure_detects_all_supported_failure_signals() {
    let cases = [
        parse_record(json!({
            "message": {
                "content": [{
                    "type": "tool_result",
                    "is_error": true,
                    "content": "ok"
                }]
            },
            "toolUseResult": {
                "stderr": "",
                "interrupted": false
            }
        })),
        parse_record(json!({
            "message": {
                "content": [{
                    "type": "tool_result",
                    "is_error": false,
                    "content": "ok"
                }]
            },
            "toolUseResult": {
                "stderr": "command failed",
                "interrupted": false
            }
        })),
        parse_record(json!({
            "message": {
                "content": [{
                    "type": "tool_result",
                    "is_error": false,
                    "content": "ok"
                }]
            },
            "toolUseResult": {
                "stderr": "",
                "interrupted": true
            }
        })),
        parse_record(json!({
            "message": {
                "content": [{
                    "type": "tool_result",
                    "is_error": false,
                    "content": "test result: FAILED. 1 passed; 1 failed"
                }]
            },
            "toolUseResult": {
                "stderr": "",
                "interrupted": false
            }
        })),
    ];

    for record in cases {
        assert!(is_bash_failure(&record));
    }
}

#[test]
fn is_bash_failure_ignores_successful_records() {
    let record = parse_record(json!({
        "message": {
            "content": [{
                "type": "tool_result",
                "is_error": false,
                "content": "all good"
            }]
        },
        "toolUseResult": {
            "stderr": "",
            "interrupted": false
        }
    }));

    assert!(!is_bash_failure(&record));
}
