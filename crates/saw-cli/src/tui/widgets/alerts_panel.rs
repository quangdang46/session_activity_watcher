use crate::cmd::common::{display_path, Snapshot};
use chrono::{DateTime, Utc};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use saw_core::AgentPhase;
use saw_daemon::alert_log_path;
use std::{fs, path::Path, time::Duration};

const MAX_HISTORY_ENTRIES: usize = 5;
const TEST_LOOP_THRESHOLD: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AlertSeverity {
    Ok,
    Warning,
    Critical,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveAlert {
    key: &'static str,
    severity: AlertSeverity,
    headline: String,
    summary: String,
    actions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AlertHistoryEntry {
    timestamp: String,
    summary: String,
}

pub struct AlertsPanel {
    lines: Vec<Line<'static>>,
    severity: AlertSeverity,
}

impl AlertsPanel {
    pub fn from_snapshot(snapshot: &Snapshot) -> Self {
        Self::from_snapshot_at(snapshot, Utc::now())
    }

    fn from_snapshot_at(snapshot: &Snapshot, now: DateTime<Utc>) -> Self {
        let alerts = active_alerts(snapshot, now);
        let history = load_alert_history(&snapshot.cwd);
        let severity = alerts
            .iter()
            .map(|alert| alert.severity)
            .max()
            .unwrap_or(AlertSeverity::Ok);

        Self {
            lines: build_lines(&alerts, &history),
            severity,
        }
    }

    pub fn render(self, scroll: u16) -> Paragraph<'static> {
        Paragraph::new(self.lines).scroll((scroll, 0)).block(
            Block::default().borders(Borders::ALL).title(Span::styled(
                "Alerts",
                Style::default()
                    .fg(self.severity.color())
                    .add_modifier(Modifier::BOLD),
            )),
        )
    }

    pub fn content_height(&self) -> usize {
        self.lines.len()
    }

    pub fn max_scroll(&self, area_height: u16) -> u16 {
        let visible_height = area_height.saturating_sub(2) as usize;
        self.content_height().saturating_sub(visible_height) as u16
    }
}

impl AlertSeverity {
    fn color(self) -> Color {
        match self {
            Self::Ok => Color::Green,
            Self::Warning => Color::Yellow,
            Self::Critical => Color::Red,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Warning => "WARNING",
            Self::Critical => "CRITICAL",
        }
    }
}

fn build_lines(alerts: &[ActiveAlert], history: &[AlertHistoryEntry]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    if alerts.is_empty() {
        lines.push(Line::from(Span::styled(
            "✓ No anomalies detected",
            Style::default()
                .fg(AlertSeverity::Ok.color())
                .add_modifier(Modifier::BOLD),
        )));
    } else {
        for (index, alert) in alerts.iter().enumerate() {
            lines.push(alert_banner_line(alert));
            lines.push(Line::from(alert.summary.clone()));
            if index + 1 < alerts.len() {
                lines.push(Line::default());
            }
        }

        let actions = recommended_actions(alerts);
        if !actions.is_empty() {
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(
                "RECOMMENDED ACTIONS",
                Style::default()
                    .fg(AlertSeverity::Warning.color())
                    .add_modifier(Modifier::BOLD),
            )));
            for (index, action) in actions.iter().enumerate() {
                lines.push(Line::from(format!("{}. {}", index + 1, action)));
            }
        }
    }

    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "RECENT ALERT HISTORY",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));

    if history.is_empty() {
        lines.push(Line::from(Span::styled(
            "No recent alerts recorded.",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for entry in history {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{} ", entry.timestamp),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(entry.summary.clone()),
            ]));
        }
    }

    lines
}

fn alert_banner_line(alert: &ActiveAlert) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!(" {} ", alert.severity.label()),
            Style::default()
                .fg(alert.severity.color())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            alert.headline.clone(),
            Style::default()
                .fg(alert.severity.color())
                .add_modifier(Modifier::BOLD),
        ),
    ])
}

fn recommended_actions(alerts: &[ActiveAlert]) -> Vec<String> {
    let mut actions = Vec::new();

    for alert in alerts {
        for action in &alert.actions {
            if !actions.iter().any(|existing| existing == action) {
                actions.push(action.clone());
            }
        }
    }

    actions
}

fn active_alerts(snapshot: &Snapshot, now: DateTime<Utc>) -> Vec<ActiveAlert> {
    let mut alerts = Vec::new();

    match &snapshot.phase {
        AgentPhase::ApiHang(duration) => push_unique_alert(&mut alerts, api_hang_alert(*duration)),
        AgentPhase::ToolLoop { file, count, since } => {
            let duration = now
                .signed_duration_since(*since)
                .to_std()
                .unwrap_or_default();
            push_unique_alert(
                &mut alerts,
                tool_loop_alert(snapshot, file, *count, duration),
            );
        }
        AgentPhase::TestLoop {
            command,
            failure_count,
        } => push_unique_alert(
            &mut alerts,
            test_loop_alert(command.clone(), *failure_count),
        ),
        AgentPhase::TaskBlocked {
            task_id,
            blocked_by,
        } => push_unique_alert(
            &mut alerts,
            task_blocked_alert(task_id.clone(), blocked_by.clone()),
        ),
        AgentPhase::ContextReset => push_unique_alert(&mut alerts, context_reset_alert(snapshot)),
        AgentPhase::ScopeLeaking {
            violating_file,
            guard_path,
        } => push_unique_alert(
            &mut alerts,
            scope_leak_alert(snapshot, violating_file, guard_path),
        ),
        AgentPhase::Dead => push_unique_alert(&mut alerts, dead_alert(snapshot)),
        _ => {}
    }

    if snapshot.state.scope_violation_count > 0 {
        let violating_file = snapshot
            .state
            .latest_scope_violation
            .as_ref()
            .cloned()
            .unwrap_or_else(|| snapshot.cwd.clone());
        let guard_path = snapshot
            .state
            .guard_paths
            .first()
            .cloned()
            .unwrap_or_else(|| snapshot.cwd.clone());
        push_unique_alert(
            &mut alerts,
            scope_leak_alert(snapshot, &violating_file, &guard_path),
        );
    }

    if snapshot.state.consecutive_test_failures >= TEST_LOOP_THRESHOLD {
        push_unique_alert(
            &mut alerts,
            test_loop_alert(
                snapshot
                    .state
                    .last_test_command
                    .clone()
                    .unwrap_or_else(|| "test command".to_string()),
                snapshot.state.consecutive_test_failures,
            ),
        );
    }

    if let Some(file) = snapshot.state.last_loop_file_path.as_ref() {
        let count = snapshot.state.recent_file_write_count(file);
        if count >= 3 {
            let duration = snapshot
                .state
                .recent_file_write_started_at(file)
                .and_then(|since| now.signed_duration_since(since).to_std().ok())
                .unwrap_or_default();
            push_unique_alert(
                &mut alerts,
                tool_loop_alert(snapshot, file, count, duration),
            );
        }
    }

    if matches!(&snapshot.phase, AgentPhase::Dead) || !snapshot.state.process_alive {
        push_unique_alert(&mut alerts, dead_alert(snapshot));
    }

    alerts.sort_by(|left, right| {
        right
            .severity
            .cmp(&left.severity)
            .then_with(|| left.headline.cmp(&right.headline))
    });
    alerts
}

fn push_unique_alert(alerts: &mut Vec<ActiveAlert>, alert: ActiveAlert) {
    if !alerts.iter().any(|existing| existing.key == alert.key) {
        alerts.push(alert);
    }
}

fn api_hang_alert(duration: Duration) -> ActiveAlert {
    ActiveAlert {
        key: "api_hang",
        severity: AlertSeverity::Warning,
        headline: format!("API_HANG • {}", format_duration(duration)),
        summary: "Claude has waited on the API without visible progress.".to_string(),
        actions: vec![
            "Press [k] to interrupt the blocked session.".to_string(),
            "Press [c] first if you want a checkpoint of recent work.".to_string(),
            "If it stays stuck, press [K] and restart the session.".to_string(),
        ],
    }
}

fn tool_loop_alert(
    snapshot: &Snapshot,
    file: &Path,
    count: u32,
    duration: Duration,
) -> ActiveAlert {
    ActiveAlert {
        key: "tool_loop",
        severity: AlertSeverity::Warning,
        headline: format!("TOOL_LOOP • {}", format_duration(duration)),
        summary: format!(
            "{} rewrites on {} without forward progress.",
            count,
            display_path(&snapshot.cwd, file)
        ),
        actions: vec![
            "Inspect the repeated write target before letting the session continue.".to_string(),
            "Press [c] to save a checkpoint before interrupting the loop.".to_string(),
            "Press [k] to stop the loop and restart from the checkpoint if needed.".to_string(),
        ],
    }
}

fn test_loop_alert(command: String, failure_count: u32) -> ActiveAlert {
    ActiveAlert {
        key: "test_loop",
        severity: AlertSeverity::Warning,
        headline: format!("TEST_LOOP • {} failures", failure_count),
        summary: format!("Repeated failing command: {command}"),
        actions: vec![
            "Read the failing command output and fix the underlying error.".to_string(),
            "Do not rerun the same test command until the failure is addressed.".to_string(),
            "Press [k] if the session keeps retrying the same test.".to_string(),
        ],
    }
}

fn task_blocked_alert(task_id: String, blocked_by: Vec<String>) -> ActiveAlert {
    ActiveAlert {
        key: "task_blocked",
        severity: AlertSeverity::Warning,
        headline: format!("TASK_BLOCKED • task {}", task_id),
        summary: format!("Blocking tasks not completed: {}", blocked_by.join(", ")),
        actions: vec![
            "Complete or reassign the blocking dependency before continuing.".to_string(),
            "Avoid retrying the blocked task until its prerequisites are done.".to_string(),
        ],
    }
}

fn context_reset_alert(snapshot: &Snapshot) -> ActiveAlert {
    let compact_count = snapshot.state.compact_count.max(1);
    ActiveAlert {
        key: "context_reset",
        severity: AlertSeverity::Warning,
        headline: format!("CONTEXT_RESET • {} compacts", compact_count),
        summary: "Context was compacted - earlier constraints may be lost".to_string(),
        actions: vec![
            "Restate the most important constraints before the session continues.".to_string(),
            "Review the next tool call to confirm the agent resumed the right task.".to_string(),
        ],
    }
}

fn scope_leak_alert(snapshot: &Snapshot, violating_file: &Path, guard_path: &Path) -> ActiveAlert {
    ActiveAlert {
        key: "scope_leak",
        severity: AlertSeverity::Warning,
        headline: format!(
            "SCOPE_LEAKING • {} violations",
            snapshot.state.scope_violation_count.max(1)
        ),
        summary: format!(
            "{} is outside the guard path {}.",
            display_path(&snapshot.cwd, violating_file),
            display_path(&snapshot.cwd, guard_path)
        ),
        actions: vec![
            "Review the out-of-scope file and decide whether it should be reverted.".to_string(),
            "Press [g] to tighten the guard path if the session is drifting.".to_string(),
            "Interrupt the session before it edits more files outside scope.".to_string(),
        ],
    }
}

fn dead_alert(snapshot: &Snapshot) -> ActiveAlert {
    let summary = if snapshot.state.process_alive {
        "The session is unresponsive even though the process still exists.".to_string()
    } else {
        "The Claude process is no longer alive.".to_string()
    };

    ActiveAlert {
        key: "dead",
        severity: AlertSeverity::Critical,
        headline: format!("DEAD • {}", format_duration(snapshot.silence)),
        summary,
        actions: vec![
            "Press [K] to hard kill the stuck process if it is still alive.".to_string(),
            "Restart the Claude session in this project.".to_string(),
            "Resume from the last checkpoint or recent file activity.".to_string(),
        ],
    }
}

fn load_alert_history(cwd: &Path) -> Vec<AlertHistoryEntry> {
    let Ok(contents) = fs::read_to_string(alert_log_path(cwd)) else {
        return Vec::new();
    };

    contents
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(MAX_HISTORY_ENTRIES)
        .map(parse_alert_history_line)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn parse_alert_history_line(line: &str) -> AlertHistoryEntry {
    let mut parts = line.splitn(4, ' ');
    let raw_timestamp = parts.next().unwrap_or("-");
    let _ = parts.next();
    let phase = parts.next().unwrap_or("ALERT");
    let rest = parts.next().unwrap_or("");
    let summary = rest
        .split(" action=")
        .next()
        .unwrap_or(rest)
        .trim()
        .to_string();

    AlertHistoryEntry {
        timestamp: format_history_timestamp(raw_timestamp),
        summary: format!("{phase} {summary}").trim().to_string(),
    }
}

fn format_history_timestamp(raw_timestamp: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(raw_timestamp)
        .map(|timestamp| timestamp.format("%H:%M:%S").to_string())
        .unwrap_or_else(|_| raw_timestamp.to_string())
}

fn format_duration(duration: Duration) -> String {
    let total_secs = duration.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;

    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::{AlertSeverity, AlertsPanel};
    use crate::cmd::common::Snapshot;
    use chrono::{TimeZone, Utc};
    use saw_core::{AgentPhase, AgentState};
    use std::{fs, path::PathBuf, time::Duration};

    fn snapshot_with_state(cwd: PathBuf, state: AgentState, phase: AgentPhase) -> Snapshot {
        Snapshot {
            cwd,
            session_file: None,
            pid: Some(42),
            state,
            phase,
            silence: Duration::from_secs(0),
        }
    }

    fn line_texts(panel: &AlertsPanel) -> Vec<String> {
        panel
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn normal_state_shows_checkmark() {
        let panel = AlertsPanel::from_snapshot_at(
            &snapshot_with_state(
                PathBuf::from("/repo"),
                AgentState::default(),
                AgentPhase::Working,
            ),
            ts(12, 0, 0),
        );

        assert_eq!(panel.severity, AlertSeverity::Ok);
        assert!(line_texts(&panel)
            .iter()
            .any(|line| line.contains("✓ No anomalies detected")));
    }

    #[test]
    fn warning_state_shows_multiple_alerts_and_numbered_actions() {
        let state = AgentState {
            scope_violation_count: 2,
            latest_scope_violation: Some(PathBuf::from("/repo/src/billing/mod.rs")),
            guard_paths: vec![PathBuf::from("/repo/src/auth")],
            consecutive_test_failures: 3,
            last_test_command: Some("cargo test -p saw-core".into()),
            ..Default::default()
        };

        let panel = AlertsPanel::from_snapshot_at(
            &snapshot_with_state(
                PathBuf::from("/repo"),
                state,
                AgentPhase::ApiHang(Duration::from_secs(125)),
            ),
            ts(12, 0, 0),
        );
        let lines = line_texts(&panel);

        assert_eq!(panel.severity, AlertSeverity::Warning);
        assert!(lines.iter().any(|line| line.contains("API_HANG • 2m 5s")));
        assert!(lines
            .iter()
            .any(|line| line.contains("SCOPE_LEAKING • 2 violations")));
        assert!(lines
            .iter()
            .any(|line| line.contains("TEST_LOOP • 3 failures")));
        assert!(lines
            .iter()
            .any(|line| line.contains("RECOMMENDED ACTIONS")));
        assert!(lines.iter().any(|line| line.starts_with("1. ")));
    }

    #[test]
    fn task_blocked_state_shows_dependency_alert() {
        let cwd = PathBuf::from("/repo");
        let panel = AlertsPanel::from_snapshot_at(
            &snapshot_with_state(
                cwd,
                AgentState::default(),
                AgentPhase::TaskBlocked {
                    task_id: "3".into(),
                    blocked_by: vec!["2".into(), "4".into()],
                },
            ),
            ts(12, 0, 0),
        );
        let lines = line_texts(&panel);

        assert!(lines
            .iter()
            .any(|line| line.contains("TASK_BLOCKED • task 3")));
        assert!(lines
            .iter()
            .any(|line| line.contains("Blocking tasks not completed: 2, 4")));
    }

    #[test]
    fn context_reset_state_warns_about_lost_constraints() {
        let cwd = PathBuf::from("/repo");
        let panel = AlertsPanel::from_snapshot_at(
            &snapshot_with_state(
                cwd,
                AgentState {
                    compact_count: 2,
                    ..Default::default()
                },
                AgentPhase::ContextReset,
            ),
            ts(12, 0, 0),
        );
        let lines = line_texts(&panel);

        assert_eq!(panel.severity, AlertSeverity::Warning);
        assert!(lines
            .iter()
            .any(|line| line.contains("CONTEXT_RESET • 2 compacts")));
        assert!(lines.iter().any(|line| {
            line.contains("Context was compacted - earlier constraints may be lost")
        }));
    }

    #[test]
    fn critical_state_shows_last_five_history_entries() {
        let cwd = unique_temp_dir("alerts-history");
        let log_path = cwd.join(".saw/alerts.log");
        fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        fs::write(
            &log_path,
            [
                history_line("2026-03-24T12:00:00Z", "API_HANG first"),
                history_line("2026-03-24T12:00:01Z", "API_HANG second"),
                history_line("2026-03-24T12:00:02Z", "TOOL_LOOP third"),
                history_line("2026-03-24T12:00:03Z", "TEST_LOOP fourth"),
                history_line("2026-03-24T12:00:04Z", "SCOPE_LEAKING fifth"),
                history_line("2026-03-24T12:00:05Z", "DEAD sixth"),
            ]
            .join("\n"),
        )
        .unwrap();

        let panel = AlertsPanel::from_snapshot_at(
            &snapshot_with_state(cwd, AgentState::default(), AgentPhase::Dead),
            ts(12, 5, 0),
        );
        let lines = line_texts(&panel);

        assert_eq!(panel.severity, AlertSeverity::Critical);
        assert!(lines.iter().any(|line| line.contains("DEAD • 0s")));
        assert!(!lines.iter().any(|line| line.contains("first")));
        assert!(lines
            .iter()
            .any(|line| line.contains("12:00:01 API_HANG second")));
        assert!(lines
            .iter()
            .any(|line| line.contains("12:00:05 DEAD sixth")));
    }

    #[test]
    fn max_scroll_accounts_for_borders() {
        let panel = AlertsPanel::from_snapshot_at(
            &snapshot_with_state(
                PathBuf::from("/repo"),
                AgentState::default(),
                AgentPhase::Working,
            ),
            ts(12, 0, 0),
        );

        assert_eq!(panel.max_scroll(10), 0);
    }

    fn ts(hour: u32, minute: u32, second: u32) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 24, hour, minute, second)
            .single()
            .expect("valid timestamp")
    }

    fn history_line(timestamp: &str, message: &str) -> String {
        format!("{timestamp} ALERT {message} action=warn")
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("saw-{prefix}-{unique}"));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
