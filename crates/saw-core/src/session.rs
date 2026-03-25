use crate::types::{AgentEvent, TokenActivity, ToolCall, ToolLineChange};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;

pub struct SessionRecord;

impl SessionRecord {
    pub fn parse(line: &str) -> Option<AgentEvent> {
        let record: JsonlRecord = serde_json::from_str(line).ok()?;
        let timestamp = record.timestamp()?;
        let is_sidechain = record.is_sidechain.unwrap_or(false);

        match record.record_type().as_deref() {
            Some("session_started") => Some(AgentEvent::SessionStart {
                timestamp,
                session_id: record.session_id()?,
            }),
            Some("assistant") => parse_assistant_record(&record, timestamp, is_sidechain),
            Some("user") => Some(parse_user_record(&record, timestamp, is_sidechain)),
            Some("system") if record.is_compact_boundary() => Some(AgentEvent::CompactBoundary {
                timestamp,
                is_sidechain,
            }),
            Some(other) if other.contains("compact") || record.is_compact_boundary() => {
                Some(AgentEvent::CompactBoundary {
                    timestamp,
                    is_sidechain,
                })
            }
            Some(
                "progress"
                | "file-history-snapshot"
                | "queue-operation"
                | "last-prompt"
                | "custom-title"
                | "agent-name",
            ) => None,
            _ if record.is_compact_boundary() => Some(AgentEvent::CompactBoundary {
                timestamp,
                is_sidechain,
            }),
            _ => None,
        }
    }
}

fn parse_assistant_record(
    record: &JsonlRecord,
    timestamp: DateTime<Utc>,
    is_sidechain: bool,
) -> Option<AgentEvent> {
    if let Some(item) = record.message_items().and_then(|items| {
        items
            .iter()
            .find(|item| item.item_type.as_deref() == Some("tool_use"))
    }) {
        let tool_name = item.name.clone().unwrap_or_else(|| "unknown".to_string());
        return Some(AgentEvent::ToolCall(ToolCall {
            timestamp,
            tool_name: tool_name.clone(),
            file_path: item
                .input
                .as_ref()
                .and_then(extract_file_path)
                .map(PathBuf::from),
            command: item.input.as_ref().and_then(extract_command),
            line_change: item.input.as_ref().and_then(extract_line_change),
            is_error: false,
            is_write: is_write_tool_name(&tool_name),
            is_sidechain,
        }));
    }

    let message = record.message.as_ref()?;
    let usage = message.usage.as_ref()?;
    Some(AgentEvent::TokenActivity(TokenActivity {
        timestamp,
        input_tokens: usage.input_tokens.unwrap_or_default(),
        output_tokens: usage.output_tokens.unwrap_or_default(),
        stop_reason: message.stop_reason.clone(),
        is_sidechain,
    }))
}

fn parse_user_record(
    record: &JsonlRecord,
    timestamp: DateTime<Utc>,
    is_sidechain: bool,
) -> AgentEvent {
    if let Some(item) = record.message_items().and_then(|items| {
        items
            .iter()
            .find(|item| item.item_type.as_deref() == Some("tool_result"))
    }) {
        let output = item
            .content
            .as_ref()
            .and_then(extract_tool_result_content)
            .or_else(|| {
                record
                    .tool_use_result
                    .as_ref()
                    .and_then(|result| result.stdout.clone())
            });

        return AgentEvent::ToolResult {
            timestamp,
            tool_name: None,
            is_error: is_tool_result_failure(item, record.tool_use_result.as_ref()),
            output: output.clone(),
            stderr: record
                .tool_use_result
                .as_ref()
                .and_then(|result| result.stderr.clone()),
            interrupted: record
                .tool_use_result
                .as_ref()
                .and_then(|result| result.interrupted)
                .unwrap_or(false),
            persisted_output_path: output.as_deref().and_then(extract_persisted_output_path),
            is_sidechain,
        };
    }

    AgentEvent::UserMessage {
        timestamp,
        is_sidechain,
    }
}

fn extract_file_path(input: &Value) -> Option<String> {
    input
        .get("file_path")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            input
                .get("notebook_path")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
}

fn extract_command(input: &Value) -> Option<String> {
    input
        .get("command")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn extract_line_change(input: &Value) -> Option<ToolLineChange> {
    let added = input
        .get("content")
        .and_then(Value::as_str)
        .map(count_lines)
        .or_else(|| {
            input
                .get("new_string")
                .and_then(Value::as_str)
                .map(count_lines)
        });
    let removed = input
        .get("old_string")
        .and_then(Value::as_str)
        .map(count_lines)
        .or_else(|| {
            input
                .get("edit")
                .and_then(|edit| edit.get("old_string"))
                .and_then(Value::as_str)
                .map(count_lines)
        });

    match (added, removed) {
        (None, None) => None,
        (added, removed) => Some(ToolLineChange {
            added: added.unwrap_or(0),
            removed: removed.unwrap_or(0),
        }),
    }
}

fn count_lines(text: &str) -> u32 {
    if text.is_empty() {
        0
    } else {
        text.lines().count() as u32
    }
}

fn extract_tool_result_content(content: &Value) -> Option<String> {
    let mut strings = Vec::new();
    collect_text(content, &mut strings);

    if strings.is_empty() {
        None
    } else {
        Some(strings.join("\n"))
    }
}

fn collect_text(value: &Value, strings: &mut Vec<String>) {
    match value {
        Value::String(text) => {
            if !text.is_empty() {
                strings.push(text.clone());
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_text(value, strings);
            }
        }
        Value::Object(map) => {
            if let Some(text) = map.get("text").and_then(Value::as_str) {
                if !text.is_empty() {
                    strings.push(text.to_string());
                }
                return;
            }

            if let Some(content) = map.get("content") {
                collect_text(content, strings);
                return;
            }

            for (key, value) in map {
                if key != "type" {
                    collect_text(value, strings);
                }
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn extract_persisted_output_path(content: &str) -> Option<PathBuf> {
    if !content.contains("<persisted-output>") {
        return None;
    }

    let marker = "Full output saved to:";
    let (_, rest) = content.split_once(marker)?;
    let path = rest.lines().map(str::trim).find(|line| {
        !line.is_empty() && !line.starts_with("Preview") && *line != "</persisted-output>"
    })?;

    Some(PathBuf::from(path))
}

fn is_tool_result_failure(
    item: &JsonlMessageItem,
    tool_use_result: Option<&ToolUseResultEnvelope>,
) -> bool {
    item.is_error.unwrap_or(false)
        || tool_use_result
            .and_then(|result| result.stderr.as_deref())
            .is_some_and(|stderr| !stderr.trim().is_empty())
        || tool_use_result
            .and_then(|result| result.interrupted)
            .unwrap_or(false)
        || item
            .content
            .as_ref()
            .and_then(extract_tool_result_content)
            .is_some_and(contains_failure_pattern)
}

fn contains_failure_pattern(content: String) -> bool {
    ["FAILED", "error[E", "panicked", "ERRORS", "npm ERR!"]
        .into_iter()
        .any(|pattern| content.contains(pattern))
}

fn is_write_tool_name(tool_name: &str) -> bool {
    matches!(
        tool_name.to_ascii_lowercase().as_str(),
        "edit" | "write" | "notebookedit" | "multiedit"
    )
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct JsonlRecord {
    #[serde(rename = "type")]
    record_type: Option<String>,
    kind: Option<String>,
    subtype: Option<String>,
    ts: Option<DateTime<Utc>>,
    timestamp: Option<DateTime<Utc>>,
    session_id: Option<String>,
    #[serde(rename = "sessionId")]
    session_id_top: Option<String>,
    #[serde(rename = "isSidechain")]
    is_sidechain: Option<bool>,
    message: Option<JsonlMessage>,
    #[serde(rename = "toolUseResult")]
    tool_use_result: Option<ToolUseResultEnvelope>,
}

impl JsonlRecord {
    fn timestamp(&self) -> Option<DateTime<Utc>> {
        self.timestamp.or(self.ts)
    }

    fn session_id(&self) -> Option<String> {
        self.session_id
            .clone()
            .or_else(|| self.session_id_top.clone())
    }

    fn record_type(&self) -> Option<String> {
        self.record_type.clone().or_else(|| self.kind.clone())
    }

    fn is_compact_boundary(&self) -> bool {
        matches!(self.subtype.as_deref(), Some("compact_boundary"))
            || matches!(self.kind.as_deref(), Some("compact_boundary"))
    }

    fn message_items(&self) -> Option<&[JsonlMessageItem]> {
        match self.message.as_ref()?.content.as_ref()? {
            JsonlMessageContent::Items(items) => Some(items.as_slice()),
            JsonlMessageContent::Text(_) => None,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct JsonlMessage {
    content: Option<JsonlMessageContent>,
    usage: Option<JsonlUsage>,
    #[serde(rename = "stop_reason")]
    stop_reason: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum JsonlMessageContent {
    Items(Vec<JsonlMessageItem>),
    Text(String),
}

impl Default for JsonlMessageContent {
    fn default() -> Self {
        Self::Text(String::new())
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct JsonlMessageItem {
    #[serde(rename = "type")]
    item_type: Option<String>,
    name: Option<String>,
    input: Option<Value>,
    content: Option<Value>,
    is_error: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct JsonlUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ToolUseResultEnvelope {
    stdout: Option<String>,
    stderr: Option<String>,
    interrupted: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::SessionRecord;
    use crate::types::AgentEvent;
    use std::path::PathBuf;

    fn parse_event(line: &str) -> AgentEvent {
        SessionRecord::parse(line).unwrap_or_else(|| panic!("failed to parse line: {line}"))
    }

    #[test]
    fn parses_session_started() {
        let line = r#"{"ts":"2026-03-24T05:41:20.047714044Z","kind":"session_started","type":"session_started","sessionId":"ses-123"}"#;
        let event = parse_event(line);
        match event {
            AgentEvent::SessionStart { session_id, .. } => assert_eq!(session_id, "ses-123"),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn parses_assistant_tool_use() {
        let line = r#"{"type":"assistant","timestamp":"2026-03-24T12:11:40.700Z","isSidechain":true,"message":{"stop_reason":"tool_use","usage":{"input_tokens":123,"output_tokens":45},"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test","file_path":"/tmp/a.txt"}}]}}"#;
        let event = parse_event(line);
        match event {
            AgentEvent::ToolCall(tool_call) => {
                assert_eq!(tool_call.tool_name, "Bash");
                assert_eq!(tool_call.file_path, Some(PathBuf::from("/tmp/a.txt")));
                assert_eq!(tool_call.command.as_deref(), Some("cargo test"));
                assert!(!tool_call.is_error);
                assert!(!tool_call.is_write);
                assert!(tool_call.is_sidechain);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn parses_assistant_token_usage_without_tool_use() {
        let line = r#"{"type":"assistant","timestamp":"2026-03-24T12:11:40.700Z","message":{"stop_reason":"end_turn","usage":{"input_tokens":1234,"output_tokens":56},"content":[{"type":"text","text":"done"}]}}"#;
        let event = parse_event(line);
        match event {
            AgentEvent::TokenActivity(activity) => {
                assert_eq!(activity.input_tokens, 1234);
                assert_eq!(activity.output_tokens, 56);
                assert_eq!(activity.stop_reason.as_deref(), Some("end_turn"));
                assert!(!activity.is_sidechain);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn parses_tool_result() {
        let line = r#"{"type":"user","timestamp":"2026-03-24T12:11:22.059Z","message":{"content":[{"type":"tool_result","tool_use_id":"call_1","is_error":true,"content":"oops"}]}}"#;
        let event = parse_event(line);
        match event {
            AgentEvent::ToolResult {
                is_error,
                output,
                stderr,
                interrupted,
                persisted_output_path,
                is_sidechain,
                ..
            } => {
                assert!(is_error);
                assert_eq!(output.as_deref(), Some("oops"));
                assert_eq!(stderr, None);
                assert!(!interrupted);
                assert_eq!(persisted_output_path, None);
                assert!(!is_sidechain);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn parses_bash_failure_from_tool_use_result_envelope() {
        let line = r#"{"type":"user","timestamp":"2026-03-24T12:11:22.059Z","message":{"content":[{"type":"tool_result","tool_use_id":"call_1","is_error":false,"content":"ok"}]},"toolUseResult":{"stdout":"ok","stderr":"boom","interrupted":false}}"#;
        let event = parse_event(line);
        match event {
            AgentEvent::ToolResult {
                is_error,
                output,
                stderr,
                interrupted,
                persisted_output_path,
                ..
            } => {
                assert!(is_error);
                assert_eq!(output.as_deref(), Some("ok"));
                assert_eq!(stderr.as_deref(), Some("boom"));
                assert!(!interrupted);
                assert_eq!(persisted_output_path, None);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn parses_sidechain_and_persisted_output_pointer() {
        let line = r#"{"type":"user","isSidechain":true,"timestamp":"2026-03-24T12:11:22.059Z","message":{"content":[{"type":"tool_result","tool_use_id":"call_1","is_error":false,"content":"<persisted-output>\nOutput too large. Full output saved to:\n/tmp/tool-results/call_1.txt\n\nPreview:\n...\n</persisted-output>"}]},"toolUseResult":{"stderr":"","interrupted":false}}"#;
        let event = parse_event(line);
        match event {
            AgentEvent::ToolResult {
                output,
                persisted_output_path,
                is_sidechain,
                ..
            } => {
                assert!(output.unwrap().contains("persisted-output"));
                assert_eq!(
                    persisted_output_path,
                    Some(PathBuf::from("/tmp/tool-results/call_1.txt"))
                );
                assert!(is_sidechain);
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn parses_plain_user_message() {
        let line = r#"{"type":"user","timestamp":"2026-03-24T12:11:05.949Z","isSidechain":false,"message":{"content":"hello"}}"#;
        let event = parse_event(line);
        match event {
            AgentEvent::UserMessage { is_sidechain, .. } => assert!(!is_sidechain),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn parses_compact_boundary() {
        let line = r#"{"type":"system","subtype":"compact_boundary","timestamp":"2026-03-24T12:11:22.059Z","isSidechain":true}"#;
        let event = parse_event(line);
        match event {
            AgentEvent::CompactBoundary { is_sidechain, .. } => assert!(is_sidechain),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn parses_compact_boundary_from_kind_without_system_type() {
        let line = r#"{"kind":"compact_boundary","timestamp":"2026-03-24T12:11:22.059Z"}"#;
        let event = parse_event(line);
        match event {
            AgentEvent::CompactBoundary { is_sidechain, .. } => assert!(!is_sidechain),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn ignores_non_event_record_types_without_panicking() {
        for line in [
            r#"{"type":"progress","timestamp":"2026-03-24T12:11:22.059Z"}"#,
            r#"{"type":"file-history-snapshot","timestamp":"2026-03-24T12:11:22.059Z"}"#,
            r#"{"type":"queue-operation","timestamp":"2026-03-24T12:11:22.059Z"}"#,
            r#"{"type":"last-prompt","timestamp":"2026-03-24T12:11:22.059Z"}"#,
            r#"{"type":"custom-title","timestamp":"2026-03-24T12:11:22.059Z"}"#,
            r#"{"type":"agent-name","timestamp":"2026-03-24T12:11:22.059Z"}"#,
        ] {
            assert!(
                SessionRecord::parse(line).is_none(),
                "expected None for {line}"
            );
        }
    }

    #[test]
    fn ignores_malformed_json() {
        assert!(SessionRecord::parse("{").is_none());
    }

    #[test]
    fn ignores_partial_line() {
        let line = r#"{"type":"assistant","timestamp":"2026-03-24T12:11:40.700Z""#;
        assert!(SessionRecord::parse(line).is_none());
    }
}
