use crate::types::{LoopScore, ToolCall};
use chrono::{DateTime, Duration, Utc};
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

const LOOP_WINDOW_MINUTES: i64 = 5;

pub fn compute_loop_score(recent_calls: &VecDeque<ToolCall>) -> LoopScore {
    let Some(latest_timestamp) = recent_calls.iter().map(|call| call.timestamp).max() else {
        return LoopScore::default();
    };

    let cutoff = latest_timestamp - Duration::minutes(LOOP_WINDOW_MINUTES);
    let calls_in_window: Vec<&ToolCall> = recent_calls
        .iter()
        .filter(|call| call.timestamp >= cutoff)
        .collect();

    let (file_rewrites, most_written_file) = compute_file_rewrites(&calls_in_window);
    let consecutive_test_failures = compute_consecutive_test_failures(&calls_in_window);

    LoopScore {
        file_rewrites,
        most_written_file,
        consecutive_test_failures,
    }
}

fn compute_file_rewrites(calls: &[&ToolCall]) -> (u32, Option<PathBuf>) {
    let mut rewrites_by_file: BTreeMap<PathBuf, (u32, DateTime<Utc>)> = BTreeMap::new();

    for call in calls.iter().copied().filter(|call| is_file_write(call)) {
        let path = call
            .file_path
            .as_ref()
            .expect("write-like tool calls always include a file path")
            .clone();
        let entry = rewrites_by_file.entry(path).or_insert((0, call.timestamp));
        entry.0 += 1;
        entry.1 = call.timestamp;
    }

    let mut best_file = None;
    let mut best_count = 0;
    let mut best_last_written_at = None;

    for (path, (count, last_written_at)) in rewrites_by_file {
        let is_better_match = count > best_count
            || (count == best_count
                && best_last_written_at
                    .map(|best_last_written_at| last_written_at > best_last_written_at)
                    .unwrap_or(true));

        if is_better_match {
            best_count = count;
            best_file = Some(path);
            best_last_written_at = Some(last_written_at);
        }
    }

    (best_count, best_file)
}

fn compute_consecutive_test_failures(calls: &[&ToolCall]) -> u32 {
    let mut failures = 0;

    for call in calls.iter().rev().copied() {
        if is_failed_test_call(call) {
            failures += 1;
            continue;
        }

        break;
    }

    failures
}

fn is_file_write(call: &ToolCall) -> bool {
    call.file_path.is_some()
        && matches!(
            call.tool_name.to_ascii_lowercase().as_str(),
            "edit" | "write" | "notebookedit" | "multiedit"
        )
}

fn is_failed_test_call(call: &ToolCall) -> bool {
    call.tool_name.eq_ignore_ascii_case("bash")
        && call.is_error
        && call
            .command
            .as_deref()
            .map(is_test_command)
            .unwrap_or(false)
}

fn is_test_command(command: &str) -> bool {
    let tokens = command
        .split_whitespace()
        .map(normalize_token)
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>();

    contains_sequence(&tokens, &["cargo", "test"])
        || contains_sequence(&tokens, &["cargo", "nextest"])
        || contains_sequence(&tokens, &["npm", "test"])
        || contains_sequence(&tokens, &["npm", "run", "test"])
        || contains_sequence(&tokens, &["pnpm", "test"])
        || contains_sequence(&tokens, &["pnpm", "run", "test"])
        || contains_sequence(&tokens, &["yarn", "test"])
        || contains_sequence(&tokens, &["bun", "test"])
        || contains_sequence(&tokens, &["python", "-m", "pytest"])
        || contains_sequence(&tokens, &["go", "test"])
        || tokens.iter().any(|token| token == "pytest")
}

fn normalize_token(token: &str) -> String {
    token
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | ';'))
        .to_ascii_lowercase()
}

fn contains_sequence(tokens: &[String], sequence: &[&str]) -> bool {
    tokens.windows(sequence.len()).any(|window| {
        window
            .iter()
            .map(String::as_str)
            .eq(sequence.iter().copied())
    })
}

#[cfg(test)]
mod tests {
    use super::compute_loop_score;
    use crate::types::ToolCall;
    use chrono::{Duration, TimeZone, Utc};
    use std::collections::VecDeque;
    use std::path::PathBuf;

    fn ts(minutes_after_start: i64) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 24, 12, 0, 0)
            .single()
            .unwrap()
            + Duration::minutes(minutes_after_start)
    }

    fn call(
        timestamp: chrono::DateTime<Utc>,
        tool_name: &str,
        file_path: Option<&str>,
        command: Option<&str>,
        is_error: bool,
    ) -> ToolCall {
        ToolCall {
            timestamp,
            tool_name: tool_name.to_string(),
            file_path: file_path.map(PathBuf::from),
            command: command.map(str::to_string),
            line_change: None,
            is_error,
            is_write: matches!(tool_name, "Edit" | "Write" | "NotebookEdit" | "MultiEdit"),
            is_sidechain: false,
        }
    }

    #[test]
    fn detects_single_file_loop() {
        let calls = VecDeque::from(vec![
            call(ts(0), "Edit", Some("src/lib.rs"), None, false),
            call(ts(1), "Edit", Some("src/lib.rs"), None, false),
            call(ts(4), "Write", Some("src/lib.rs"), None, false),
        ]);

        let score = compute_loop_score(&calls);

        assert_eq!(score.file_rewrites, 3);
        assert_eq!(score.most_written_file, Some(PathBuf::from("src/lib.rs")));
        assert_eq!(score.consecutive_test_failures, 0);
    }

    #[test]
    fn does_not_flag_multi_file_activity_as_loop() {
        let calls = VecDeque::from(vec![
            call(ts(0), "Edit", Some("src/a.rs"), None, false),
            call(ts(1), "Write", Some("src/b.rs"), None, false),
            call(ts(2), "Edit", Some("src/a.rs"), None, false),
        ]);

        let score = compute_loop_score(&calls);

        assert_eq!(score.file_rewrites, 2);
        assert_eq!(score.most_written_file, Some(PathBuf::from("src/a.rs")));
        assert_eq!(score.consecutive_test_failures, 0);
    }

    #[test]
    fn detects_test_loop_from_consecutive_failed_test_commands() {
        let calls = VecDeque::from(vec![
            call(ts(0), "Bash", None, Some("cargo test"), true),
            call(ts(1), "Bash", None, Some("cargo test -p saw-core"), true),
            call(ts(2), "Bash", None, Some("pytest tests/unit"), true),
        ]);

        let score = compute_loop_score(&calls);

        assert_eq!(score.file_rewrites, 0);
        assert_eq!(score.most_written_file, None);
        assert_eq!(score.consecutive_test_failures, 3);
    }

    #[test]
    fn reports_no_loop_for_mixed_activity() {
        let calls = VecDeque::from(vec![
            call(ts(0), "Bash", None, Some("cargo test"), true),
            call(ts(1), "Bash", None, Some("cargo test"), false),
            call(ts(2), "Bash", None, Some("cargo test"), true),
            call(ts(3), "Bash", None, Some("cargo fmt --check"), true),
        ]);

        let score = compute_loop_score(&calls);

        assert_eq!(score.file_rewrites, 0);
        assert_eq!(score.most_written_file, None);
        assert_eq!(score.consecutive_test_failures, 0);
    }

    #[test]
    fn ignores_calls_outside_the_five_minute_window() {
        let calls = VecDeque::from(vec![
            call(ts(0), "Edit", Some("src/lib.rs"), None, false),
            call(ts(1), "Edit", Some("src/lib.rs"), None, false),
            call(ts(7), "Edit", Some("src/lib.rs"), None, false),
            call(ts(8), "Bash", None, Some("cargo test"), true),
            call(ts(9), "Bash", None, Some("cargo test"), true),
        ]);

        let score = compute_loop_score(&calls);

        assert_eq!(score.file_rewrites, 1);
        assert_eq!(score.most_written_file, Some(PathBuf::from("src/lib.rs")));
        assert_eq!(score.consecutive_test_failures, 2);
    }
}
