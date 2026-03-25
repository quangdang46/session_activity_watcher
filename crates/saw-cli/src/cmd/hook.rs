use crate::cmd::common::append_hook_event;
use anyhow::{Context, Result};
use chrono::Utc;
use clap::Args;
use saw_core::{AgentEvent, ToolCall, ToolLineChange};
use serde::Deserialize;
use serde_json::Value;
use std::io::Read;
use std::path::PathBuf;

#[derive(Debug, Args)]
pub struct HookArgs {
    #[arg(long)]
    pub pre: bool,

    #[arg(long)]
    pub session_start: bool,

    #[arg(long, default_value = ".")]
    pub dir: PathBuf,
}

#[derive(Debug, Deserialize)]
struct HookPayload {
    hook_event_name: Option<String>,
    session_id: String,
    tool_name: Option<String>,
    tool_input: Option<Value>,
    tool_response: Option<Value>,
}

pub fn run(args: HookArgs) -> Result<()> {
    let mut stdin = String::new();
    std::io::stdin()
        .read_to_string(&mut stdin)
        .context("failed to read hook payload from stdin")?;

    let payload: HookPayload = serde_json::from_str(&stdin).context("invalid hook payload JSON")?;
    let cwd = args.dir.canonicalize().unwrap_or(args.dir);
    let event = payload_to_event(&payload, &cwd, args.pre, args.session_start);
    let path = append_hook_event(&cwd, &payload.session_id, &event)?;

    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "ok": true,
            "session_id": payload.session_id,
            "hook_log": path,
        }))?
    );

    Ok(())
}

fn payload_to_event(
    payload: &HookPayload,
    cwd: &std::path::Path,
    pre: bool,
    session_start: bool,
) -> AgentEvent {
    let timestamp = Utc::now();
    let hook_name = payload.hook_event_name.as_deref().unwrap_or_default();

    if session_start || hook_name == "SessionStart" {
        return AgentEvent::SessionStart {
            timestamp,
            session_id: payload.session_id.clone(),
        };
    }

    if pre || hook_name == "PreToolUse" {
        let tool_name = payload
            .tool_name
            .clone()
            .unwrap_or_else(|| "unknown".into());
        return AgentEvent::ToolCall(ToolCall {
            timestamp,
            tool_name: tool_name.clone(),
            file_path: payload
                .tool_input
                .as_ref()
                .and_then(|input| extract_file_path(input, cwd)),
            command: payload.tool_input.as_ref().and_then(extract_command),
            line_change: payload.tool_input.as_ref().and_then(extract_line_change),
            is_error: false,
            is_write: is_write_tool(&tool_name),
            is_sidechain: false,
        });
    }

    AgentEvent::ToolResult {
        timestamp,
        tool_name: payload.tool_name.clone(),
        is_error: tool_response_failed(payload.tool_response.as_ref()),
        output: payload.tool_response.as_ref().map(flatten_json),
        stderr: None,
        interrupted: false,
        persisted_output_path: None,
        is_sidechain: false,
    }
}

fn extract_file_path(input: &Value, cwd: &std::path::Path) -> Option<PathBuf> {
    let raw = input
        .get("file_path")
        .and_then(Value::as_str)
        .or_else(|| input.get("notebook_path").and_then(Value::as_str))?;
    let path = PathBuf::from(raw);
    Some(if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    })
}

fn extract_command(input: &Value) -> Option<String> {
    input
        .get("command")
        .and_then(Value::as_str)
        .map(str::to_string)
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

fn is_write_tool(tool_name: &str) -> bool {
    matches!(
        tool_name.to_ascii_lowercase().as_str(),
        "edit" | "write" | "notebookedit" | "multiedit"
    )
}

fn tool_response_failed(response: Option<&Value>) -> bool {
    let Some(response) = response else {
        return false;
    };

    response
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || flatten_json(response).contains("error")
}

fn flatten_json(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(values) => values
            .iter()
            .map(flatten_json)
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(map) => {
            if let Some(text) = map.get("text").and_then(Value::as_str) {
                return text.to_string();
            }
            map.values()
                .map(flatten_json)
                .collect::<Vec<_>>()
                .join("\n")
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{payload_to_event, HookPayload};
    use saw_core::AgentEvent;
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn builds_pre_tool_event() {
        let payload = HookPayload {
            hook_event_name: Some("PreToolUse".into()),
            session_id: "ses-1".into(),
            tool_name: Some("Write".into()),
            tool_input: Some(json!({"file_path":"src/lib.rs"})),
            tool_response: None,
        };

        let event = payload_to_event(&payload, Path::new("/repo"), true, false);
        match event {
            AgentEvent::ToolCall(call) => {
                assert_eq!(call.tool_name, "Write");
                assert_eq!(call.file_path.unwrap(), Path::new("/repo/src/lib.rs"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[test]
    fn builds_session_start_event() {
        let payload = HookPayload {
            hook_event_name: Some("SessionStart".into()),
            session_id: "ses-1".into(),
            tool_name: None,
            tool_input: None,
            tool_response: None,
        };

        let event = payload_to_event(&payload, Path::new("/repo"), false, true);
        match event {
            AgentEvent::SessionStart { session_id, .. } => assert_eq!(session_id, "ses-1"),
            other => panic!("unexpected event: {other:?}"),
        }
    }
}
