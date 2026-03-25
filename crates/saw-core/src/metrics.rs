use crate::types::AgentState;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{Duration, Instant};

/// Computes silence using the most recent externally visible activity source.
///
/// The state currently stores timestamps as `DateTime<Utc>`, so the helper uses
/// the same clock domain as the rest of `saw-core`.
pub fn compute_silence(state: &AgentState, now: DateTime<Utc>) -> Duration {
    [
        state.last_jsonl_record_at,
        state.last_file_event_at,
        state.last_hook_event_at,
    ]
    .into_iter()
    .flatten()
    .max()
    .or(state.last_event_at)
    .or(state.session_started_at)
    .map(|last_activity| {
        now.signed_duration_since(last_activity)
            .to_std()
            .unwrap_or_default()
    })
    .unwrap_or_default()
}

pub fn compute_io_rate(previous_bytes: u64, current_bytes: u64, elapsed: Duration) -> f32 {
    if elapsed.is_zero() {
        return 0.0;
    }

    current_bytes.saturating_sub(previous_bytes) as f32 / elapsed.as_secs_f32()
}

pub fn has_recent_io_activity(state: &AgentState, now: DateTime<Utc>, window: Duration) -> bool {
    state.last_io_activity_at.is_some_and(|last_io_activity| {
        now.signed_duration_since(last_io_activity)
            .to_std()
            .map(|elapsed| elapsed <= window)
            .unwrap_or(true)
    })
}

/// Delta produced when token totals advance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenDelta {
    pub input: u64,
    pub output: u64,
    pub elapsed: Duration,
}

/// Tracks cumulative token counters and detects when they stop moving.
#[derive(Debug, Clone, Copy)]
pub struct TokenTracker {
    last_input: u64,
    last_output: u64,
    last_seen_at: Instant,
}

impl Default for TokenTracker {
    fn default() -> Self {
        Self::new(Instant::now())
    }
}

impl TokenTracker {
    pub fn new(now: Instant) -> Self {
        Self {
            last_input: 0,
            last_output: 0,
            last_seen_at: now,
        }
    }

    pub fn update(&mut self, input: u64, output: u64) -> TokenDelta {
        self.update_at(input, output, Instant::now())
    }

    pub fn is_frozen(&self, threshold: Duration) -> bool {
        self.is_frozen_at(Instant::now(), threshold)
    }

    fn update_at(&mut self, input: u64, output: u64, now: Instant) -> TokenDelta {
        let delta = TokenDelta {
            input: input.saturating_sub(self.last_input),
            output: output.saturating_sub(self.last_output),
            elapsed: now.saturating_duration_since(self.last_seen_at),
        };

        self.last_input = input;
        self.last_output = output;
        self.last_seen_at = now;

        delta
    }

    fn is_frozen_at(&self, now: Instant, threshold: Duration) -> bool {
        now.saturating_duration_since(self.last_seen_at) > threshold
    }
}

pub fn activity_weight(is_sidechain: bool) -> f32 {
    if is_sidechain {
        0.3
    } else {
        1.0
    }
}

/// Minimal raw JSONL shape needed for metric helpers.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct JsonlRecord {
    #[serde(rename = "type")]
    pub record_type: Option<String>,
    #[serde(rename = "isSidechain")]
    pub is_sidechain: Option<bool>,
    pub message: Option<JsonlMessage>,
    #[serde(rename = "toolUseResult")]
    pub tool_use_result: Option<ToolUseResult>,
}

impl JsonlRecord {
    pub fn tool_result_is_error(&self) -> Option<bool> {
        self.message_items()?
            .iter()
            .find(|item| item.item_type.as_deref() == Some("tool_result"))
            .and_then(|item| item.is_error)
    }

    pub fn tool_result_content(&self) -> Option<String> {
        let value = self
            .message_items()?
            .iter()
            .find(|item| item.item_type.as_deref() == Some("tool_result"))?
            .content
            .as_ref()?;

        flatten_json_strings(value)
    }

    fn message_items(&self) -> Option<&[JsonlMessageItem]> {
        match self.message.as_ref()?.content.as_ref()? {
            JsonlMessageContent::Items(items) => Some(items.as_slice()),
            JsonlMessageContent::Text(_) => None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct JsonlMessage {
    pub content: Option<JsonlMessageContent>,
    pub usage: Option<JsonlUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum JsonlMessageContent {
    Text(String),
    Items(Vec<JsonlMessageItem>),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct JsonlMessageItem {
    #[serde(rename = "type")]
    pub item_type: Option<String>,
    pub is_error: Option<bool>,
    pub content: Option<Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct JsonlUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ToolUseResult {
    pub stderr: Option<String>,
    pub interrupted: Option<bool>,
}

pub fn is_bash_failure(record: &JsonlRecord) -> bool {
    if record.tool_result_is_error() == Some(true) {
        return true;
    }

    if record
        .tool_use_result
        .as_ref()
        .and_then(|result| result.stderr.as_deref())
        .is_some_and(|stderr| !stderr.trim().is_empty())
    {
        return true;
    }

    if matches!(
        record
            .tool_use_result
            .as_ref()
            .and_then(|result| result.interrupted),
        Some(true)
    ) {
        return true;
    }

    record.tool_result_content().is_some_and(|content| {
        ["FAILED", "error[E", "panicked", "ERRORS", "npm ERR!"]
            .into_iter()
            .any(|pattern| content.contains(pattern))
    })
}

fn flatten_json_strings(value: &Value) -> Option<String> {
    let mut strings = Vec::new();
    collect_strings(value, &mut strings);

    if strings.is_empty() {
        None
    } else {
        Some(strings.join("\n"))
    }
}

fn collect_strings(value: &Value, strings: &mut Vec<String>) {
    match value {
        Value::String(text) => {
            if !text.is_empty() {
                strings.push(text.clone());
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_strings(value, strings);
            }
        }
        Value::Object(map) => {
            for value in map.values() {
                collect_strings(value, strings);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{
        activity_weight, compute_io_rate, compute_silence, has_recent_io_activity, is_bash_failure,
        JsonlRecord, TokenDelta, TokenTracker,
    };
    use crate::types::AgentState;
    use chrono::{Duration as ChronoDuration, TimeZone, Utc};
    use serde_json::json;
    use std::time::{Duration, Instant};

    fn parse_record(value: serde_json::Value) -> JsonlRecord {
        serde_json::from_value(value).expect("valid JsonlRecord fixture")
    }

    fn ts(hour: u32, minute: u32, second: u32) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 24, hour, minute, second)
            .single()
            .expect("valid timestamp")
    }

    #[test]
    fn compute_silence_uses_most_recent_activity_timestamp() {
        let now = ts(12, 0, 0);
        let state = AgentState {
            last_jsonl_record_at: Some(now - ChronoDuration::seconds(40)),
            last_file_event_at: Some(now - ChronoDuration::seconds(25)),
            last_hook_event_at: Some(now - ChronoDuration::seconds(10)),
            ..Default::default()
        };

        assert_eq!(compute_silence(&state, now), Duration::from_secs(10));
    }

    #[test]
    fn compute_silence_falls_back_to_session_start() {
        let now = ts(12, 0, 0);
        let state = AgentState {
            session_started_at: Some(now - ChronoDuration::seconds(90)),
            ..Default::default()
        };

        assert_eq!(compute_silence(&state, now), Duration::from_secs(90));
    }

    #[test]
    fn compute_io_rate_uses_elapsed_seconds() {
        assert_eq!(compute_io_rate(100, 700, Duration::from_secs(3)), 200.0);
        assert_eq!(compute_io_rate(700, 100, Duration::from_secs(3)), 0.0);
        assert_eq!(compute_io_rate(100, 700, Duration::ZERO), 0.0);
    }

    #[test]
    fn has_recent_io_activity_tracks_window() {
        let now = ts(12, 0, 0);
        let mut state = AgentState {
            last_io_activity_at: Some(now - ChronoDuration::seconds(59)),
            ..Default::default()
        };
        assert!(has_recent_io_activity(&state, now, Duration::from_secs(60)));

        state.last_io_activity_at = Some(now - ChronoDuration::seconds(61));
        assert!(!has_recent_io_activity(
            &state,
            now,
            Duration::from_secs(60)
        ));
    }

    #[test]
    fn token_tracker_updates_with_elapsed_and_deltas() {
        let start = Instant::now();
        let mut tracker = TokenTracker::new(start);

        let first = tracker.update_at(12, 5, start + Duration::from_secs(3));
        let second = tracker.update_at(20, 9, start + Duration::from_secs(8));

        assert_eq!(
            first,
            TokenDelta {
                input: 12,
                output: 5,
                elapsed: Duration::from_secs(3),
            }
        );
        assert_eq!(
            second,
            TokenDelta {
                input: 8,
                output: 4,
                elapsed: Duration::from_secs(5),
            }
        );
    }

    #[test]
    fn token_tracker_detects_frozen_threshold() {
        let start = Instant::now();
        let tracker = TokenTracker::new(start);

        assert!(!tracker.is_frozen_at(start + Duration::from_secs(9), Duration::from_secs(10)));
        assert!(tracker.is_frozen_at(start + Duration::from_secs(11), Duration::from_secs(10)));
    }

    #[test]
    fn activity_weight_scales_sidechain_activity() {
        assert_eq!(activity_weight(false), 1.0);
        assert_eq!(activity_weight(true), 0.3);
    }

    #[test]
    fn bash_failure_detects_tool_result_error_flag() {
        let record = parse_record(json!({
            "type": "user",
            "message": {
                "content": [
                    {
                        "type": "tool_result",
                        "is_error": true,
                        "content": "ok"
                    }
                ]
            },
            "toolUseResult": {
                "stderr": "",
                "interrupted": false
            }
        }));

        assert!(is_bash_failure(&record));
    }

    #[test]
    fn bash_failure_detects_stderr_signal() {
        let record = parse_record(json!({
            "type": "user",
            "message": {
                "content": [
                    {
                        "type": "tool_result",
                        "is_error": false,
                        "content": "ok"
                    }
                ]
            },
            "toolUseResult": {
                "stderr": "command failed",
                "interrupted": false
            }
        }));

        assert!(is_bash_failure(&record));
    }

    #[test]
    fn bash_failure_detects_interrupted_signal() {
        let record = parse_record(json!({
            "type": "user",
            "message": {
                "content": [
                    {
                        "type": "tool_result",
                        "is_error": false,
                        "content": "ok"
                    }
                ]
            },
            "toolUseResult": {
                "stderr": "",
                "interrupted": true
            }
        }));

        assert!(is_bash_failure(&record));
    }

    #[test]
    fn bash_failure_detects_common_content_patterns() {
        for content in [
            "test result: FAILED. 1 passed; 1 failed",
            "error[E0425]: cannot find value `x` in this scope",
            "thread 'main' panicked at 'boom'",
            "ERRORS\n================",
            "npm ERR! code ELIFECYCLE",
        ] {
            let record = parse_record(json!({
                "type": "user",
                "message": {
                    "content": [
                        {
                            "type": "tool_result",
                            "is_error": false,
                            "content": content
                        }
                    ]
                },
                "toolUseResult": {
                    "stderr": "",
                    "interrupted": false
                }
            }));

            assert!(
                is_bash_failure(&record),
                "expected failure for content: {content}"
            );
        }
    }

    #[test]
    fn bash_failure_ignores_successful_records() {
        let record = parse_record(json!({
            "type": "user",
            "message": {
                "content": [
                    {
                        "type": "tool_result",
                        "is_error": false,
                        "content": "all good"
                    }
                ]
            },
            "toolUseResult": {
                "stderr": "",
                "interrupted": false
            }
        }));

        assert!(!is_bash_failure(&record));
    }
}
