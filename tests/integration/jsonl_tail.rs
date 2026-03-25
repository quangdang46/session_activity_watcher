use saw_core::AgentEvent;
use saw_daemon::{JsonlTailer, JSONL_TAIL_POLL_INTERVAL};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

#[tokio::test]
async fn emits_mixed_events_for_appended_jsonl_records() {
    let dir = unique_temp_dir("mixed");
    let file = dir.join("ses-1.jsonl");
    fs::write(&file, "").unwrap();

    let mut tailer = JsonlTailer::with_follow_newest(&file, false).unwrap();
    tailer.set_byte_offset(0);

    let (handle, mut rx) = spawn_tailer(tailer, 16);
    sleep(JSONL_TAIL_POLL_INTERVAL).await;

    append(
        &file,
        &format!(
            "{}{}{}",
            session_started_line("ses-123"),
            assistant_tool_use_line("Edit", Some("/tmp/edited.txt"), None),
            user_tool_result_line("command finished", "", false),
        ),
    );

    assert_session_start(recv_event(&mut rx).await, "ses-123");
    assert_tool_call(
        recv_event(&mut rx).await,
        "Edit",
        Some("/tmp/edited.txt"),
        None,
        false,
        true,
    );
    assert_tool_result(
        recv_event(&mut rx).await,
        Some("command finished"),
        None,
        false,
        false,
    );

    stop_tailer(handle).await;
}

#[tokio::test]
async fn buffers_partial_line_until_newline_is_written() {
    let dir = unique_temp_dir("partial");
    let file = dir.join("ses-1.jsonl");
    fs::write(&file, "").unwrap();

    let mut tailer = JsonlTailer::with_follow_newest(&file, false).unwrap();
    tailer.set_byte_offset(0);

    let (handle, mut rx) = spawn_tailer(tailer, 8);
    sleep(JSONL_TAIL_POLL_INTERVAL).await;

    append(
        &file,
        r#"{"type":"assistant","timestamp":"2026-03-24T12:11:40.700Z","message":{"stop_reason":"tool_use","usage":{"input_tokens":123,"output_tokens":45},"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/tmp/partial.txt"}}]}}"#,
    );

    assert!(timeout(Duration::from_millis(300), rx.recv())
        .await
        .is_err());

    append(&file, "\n");
    assert_tool_call(
        recv_event(&mut rx).await,
        "Edit",
        Some("/tmp/partial.txt"),
        None,
        false,
        true,
    );

    stop_tailer(handle).await;
}

#[tokio::test]
async fn handles_file_rotation_without_losing_events() {
    let dir = unique_temp_dir("rotation");
    let first = dir.join("ses-1.jsonl");
    fs::write(&first, "").unwrap();

    let mut tailer = JsonlTailer::new(&first).unwrap();
    tailer.set_byte_offset(0);

    let (handle, mut rx) = spawn_tailer(tailer, 16);
    sleep(JSONL_TAIL_POLL_INTERVAL).await;

    append(
        &first,
        &assistant_tool_use_line("Edit", Some("/tmp/first.txt"), None),
    );
    assert_tool_call(
        recv_event(&mut rx).await,
        "Edit",
        Some("/tmp/first.txt"),
        None,
        false,
        true,
    );

    sleep(Duration::from_millis(25)).await;
    let second = dir.join("ses-2.jsonl");
    fs::write(&second, user_tool_result_line("rotated", "", false)).unwrap();

    assert_tool_result(
        recv_event(&mut rx).await,
        Some("rotated"),
        None,
        false,
        false,
    );

    append(
        &second,
        &assistant_tool_use_line("Bash", None, Some("cargo test -p saw-daemon")),
    );
    assert_tool_call(
        recv_event(&mut rx).await,
        "Bash",
        None,
        Some("cargo test -p saw-daemon"),
        false,
        false,
    );

    stop_tailer(handle).await;
}

#[tokio::test]
async fn processes_large_jsonl_batches_without_missing_records() {
    let dir = unique_temp_dir("large");
    let file = dir.join("ses-1.jsonl");
    fs::write(&file, "").unwrap();

    let mut tailer = JsonlTailer::with_follow_newest(&file, false).unwrap();
    tailer.set_byte_offset(0);

    let (handle, mut rx) = spawn_tailer(tailer, 2048);
    sleep(JSONL_TAIL_POLL_INTERVAL).await;

    let mut batch = String::new();
    for index in 0..1200 {
        if index % 2 == 0 {
            batch.push_str(&assistant_tool_use_line(
                "Edit",
                Some(&format!("/tmp/file-{index}.txt")),
                None,
            ));
        } else {
            batch.push_str(&user_tool_result_line(
                &format!("result-{index}"),
                "",
                false,
            ));
        }
    }
    append(&file, &batch);

    let mut tool_calls = 0usize;
    let mut tool_results = 0usize;
    for _ in 0..1200 {
        match recv_event(&mut rx).await {
            AgentEvent::ToolCall(_) => tool_calls += 1,
            AgentEvent::ToolResult { .. } => tool_results += 1,
            other => panic!("unexpected event: {other:?}"),
        }
    }

    assert_eq!(tool_calls, 600);
    assert_eq!(tool_results, 600);

    stop_tailer(handle).await;
}

fn spawn_tailer(
    mut tailer: JsonlTailer,
    capacity: usize,
) -> (JoinHandle<()>, mpsc::Receiver<AgentEvent>) {
    let (tx, rx) = mpsc::channel(capacity);
    let handle = tokio::spawn(async move {
        tailer.run(tx).await.unwrap();
    });
    (handle, rx)
}

async fn recv_event(rx: &mut mpsc::Receiver<AgentEvent>) -> AgentEvent {
    timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("event should arrive before timeout")
        .expect("channel should stay open while tailer is running")
}

async fn stop_tailer(handle: JoinHandle<()>) {
    handle.abort();
    let _ = handle.await;
}

fn append(path: &Path, content: &str) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    file.write_all(content.as_bytes()).unwrap();
    file.sync_all().unwrap();
}

fn session_started_line(session_id: &str) -> String {
    format!(
        "{{\"type\":\"session_started\",\"timestamp\":\"2026-03-24T12:00:00Z\",\"sessionId\":\"{session_id}\"}}\n"
    )
}

fn assistant_tool_use_line(
    tool_name: &str,
    file_path: Option<&str>,
    command: Option<&str>,
) -> String {
    let mut input_fields = Vec::new();
    if let Some(file_path) = file_path {
        input_fields.push(format!("\"file_path\":\"{file_path}\""));
    }
    if let Some(command) = command {
        input_fields.push(format!("\"command\":\"{command}\""));
    }
    let input = input_fields.join(",");

    format!(
        "{{\"type\":\"assistant\",\"timestamp\":\"2026-03-24T12:11:40.700Z\",\"message\":{{\"stop_reason\":\"tool_use\",\"usage\":{{\"input_tokens\":123,\"output_tokens\":45}},\"content\":[{{\"type\":\"tool_use\",\"name\":\"{tool_name}\",\"input\":{{{input}}}}}]}}}}\n"
    )
}

fn user_tool_result_line(output: &str, stderr: &str, interrupted: bool) -> String {
    let stderr_field = if stderr.is_empty() {
        String::new()
    } else {
        format!(",\"stderr\":\"{stderr}\"")
    };

    format!(
        "{{\"type\":\"user\",\"timestamp\":\"2026-03-24T12:11:22.059Z\",\"message\":{{\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"call_1\",\"is_error\":false,\"content\":\"{output}\"}}]}},\"toolUseResult\":{{\"stdout\":\"{output}\"{stderr_field},\"interrupted\":{interrupted}}}}}\n"
    )
}

fn assert_session_start(event: AgentEvent, expected_session_id: &str) {
    match event {
        AgentEvent::SessionStart { session_id, .. } => assert_eq!(session_id, expected_session_id),
        other => panic!("unexpected event: {other:?}"),
    }
}

fn assert_tool_call(
    event: AgentEvent,
    expected_tool_name: &str,
    expected_file_path: Option<&str>,
    expected_command: Option<&str>,
    expected_is_error: bool,
    expected_is_write: bool,
) {
    match event {
        AgentEvent::ToolCall(tool_call) => {
            assert_eq!(tool_call.tool_name, expected_tool_name);
            assert_eq!(
                tool_call.file_path.as_deref(),
                expected_file_path.map(Path::new),
            );
            assert_eq!(tool_call.command.as_deref(), expected_command);
            assert_eq!(tool_call.is_error, expected_is_error);
            assert_eq!(tool_call.is_write, expected_is_write);
            assert!(!tool_call.is_sidechain);
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

fn assert_tool_result(
    event: AgentEvent,
    expected_output: Option<&str>,
    expected_stderr: Option<&str>,
    expected_interrupted: bool,
    expected_is_error: bool,
) {
    match event {
        AgentEvent::ToolResult {
            output,
            stderr,
            interrupted,
            is_error,
            is_sidechain,
            ..
        } => {
            assert_eq!(output.as_deref(), expected_output);
            assert_eq!(stderr.as_deref(), expected_stderr);
            assert_eq!(interrupted, expected_interrupted);
            assert_eq!(is_error, expected_is_error);
            assert!(!is_sidechain);
        }
        other => panic!("unexpected event: {other:?}"),
    }
}

fn unique_temp_dir(label: &str) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("saw-jsonl-tail-integration-{label}-{unique}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}
