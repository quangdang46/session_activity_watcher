use crate::cmd::common::Snapshot;
use ratatui::{
    text::Line,
    widgets::{Block, Borders, Paragraph},
};
use std::time::Duration;

pub struct MetricsPanel {
    tokens: String,
    cpu: String,
    memory: String,
    io: String,
    session: String,
    tool_calls: u64,
    files_touched: usize,
    compacts: u64,
}

impl MetricsPanel {
    pub fn from_snapshot(snapshot: &Snapshot) -> Self {
        Self::from_snapshot_at(snapshot, chrono::Utc::now())
    }

    fn from_snapshot_at(snapshot: &Snapshot, now: chrono::DateTime<chrono::Utc>) -> Self {
        let token_status = match snapshot.state.last_token_activity_at {
            Some(last_token_activity_at) => {
                let elapsed = now
                    .signed_duration_since(last_token_activity_at)
                    .to_std()
                    .unwrap_or_default();
                if elapsed <= Duration::from_secs(10) {
                    "↑ active".to_string()
                } else {
                    format!("✗ frozen ({})", format_duration(elapsed))
                }
            }
            None => "✗ frozen".to_string(),
        };
        let io_rate = snapshot
            .state
            .latest_io_read_rate
            .max(snapshot.state.latest_io_write_rate);
        let io_direction =
            if snapshot.state.latest_io_write_rate > snapshot.state.latest_io_read_rate {
                "↓"
            } else {
                "↑"
            };
        let io_text = if io_rate > 0.0 {
            format!("{}{}/s", io_direction, format_bytes(io_rate))
        } else {
            "0 B/s".to_string()
        };

        Self {
            tokens: format!(
                "in {} / out {}  {}",
                snapshot.state.total_input_tokens, snapshot.state.total_output_tokens, token_status
            ),
            cpu: format!(
                "{}  {:>3.0}%",
                cpu_bar(snapshot.state.latest_cpu_percent),
                snapshot.state.latest_cpu_percent.clamp(0.0, 100.0)
            ),
            memory: format_bytes(snapshot.state.latest_rss_bytes as f32),
            io: io_text,
            session: format_duration(
                snapshot
                    .state
                    .session_started_at
                    .and_then(|started_at| now.signed_duration_since(started_at).to_std().ok())
                    .unwrap_or_default(),
            ),
            tool_calls: snapshot.state.total_tool_calls,
            files_touched: snapshot.state.touched_file_count(),
            compacts: snapshot.state.compact_count,
        }
    }

    pub fn render(self) -> Paragraph<'static> {
        Paragraph::new(vec![
            metric_line("Tokens", self.tokens),
            metric_line("CPU", self.cpu),
            metric_line("Memory", self.memory),
            metric_line("I/O", self.io),
            Line::default(),
            metric_line("Session", self.session),
            metric_line("Tool calls", self.tool_calls.to_string()),
            metric_line("Files", format!("{} touched", self.files_touched)),
            metric_line("Compacts", self.compacts.to_string()),
        ])
        .block(Block::default().borders(Borders::ALL).title("Metrics"))
    }
}

fn metric_line(label: &'static str, value: impl Into<String>) -> Line<'static> {
    Line::from(format!("{label:<10} {}", value.into()))
}

fn cpu_bar(cpu_percent: f32) -> String {
    let filled = ((cpu_percent.clamp(0.0, 100.0) / 100.0) * 8.0).round() as usize;
    format!(
        "{}{}",
        "█".repeat(filled),
        "░".repeat(8usize.saturating_sub(filled))
    )
}

fn format_bytes(bytes: f32) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];

    let mut value = bytes.max(0.0);
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }

    if unit == 0 || value >= 10.0 {
        format!("{:.0} {}", value, UNITS[unit])
    } else {
        format!("{:.1} {}", value, UNITS[unit])
    }
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
    use super::{cpu_bar, format_bytes, format_duration, MetricsPanel};
    use crate::cmd::common::Snapshot;
    use chrono::{Duration as ChronoDuration, TimeZone, Utc};
    use saw_core::{AgentPhase, AgentState};
    use std::path::PathBuf;
    use std::time::Duration;

    fn ts(hour: u32, minute: u32, second: u32) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 24, hour, minute, second)
            .single()
            .expect("valid timestamp")
    }

    fn snapshot_with_state(state: AgentState) -> Snapshot {
        Snapshot {
            cwd: PathBuf::from("/repo"),
            session_file: None,
            pid: Some(42),
            phase: AgentPhase::Working,
            silence: Duration::from_secs(0),
            state,
        }
    }

    #[test]
    fn cpu_bar_scales_to_eight_slots() {
        assert_eq!(cpu_bar(0.0), "░░░░░░░░");
        assert_eq!(cpu_bar(12.0), "█░░░░░░░");
        assert_eq!(cpu_bar(50.0), "████░░░░");
        assert_eq!(cpu_bar(100.0), "████████");
    }

    #[test]
    fn formats_bytes_human_readably() {
        assert_eq!(format_bytes(999.0), "999 B");
        assert_eq!(format_bytes(1_536.0), "1.5 KB");
        assert_eq!(format_bytes(358_612_992.0), "342 MB");
    }

    #[test]
    fn formats_duration_human_readably() {
        assert_eq!(format_duration(Duration::from_secs(14)), "14s");
        assert_eq!(format_duration(Duration::from_secs(374)), "6m 14s");
        assert_eq!(format_duration(Duration::from_secs(7_560)), "2h 6m");
    }

    #[test]
    fn metrics_panel_marks_active_tokens_and_formats_metrics() {
        let now = ts(12, 0, 0);
        let mut state = AgentState {
            total_input_tokens: 1200,
            total_output_tokens: 340,
            total_tool_calls: 23,
            compact_count: 2,
            latest_cpu_percent: 12.0,
            latest_rss_bytes: 358_612_992,
            latest_io_read_rate: 1_228.8,
            session_started_at: Some(now - ChronoDuration::seconds(374)),
            last_token_activity_at: Some(now),
            ..Default::default()
        };
        state
            .touched_files
            .insert(PathBuf::from("/repo/src/lib.rs"));
        state
            .touched_files
            .insert(PathBuf::from("/repo/src/main.rs"));
        state
            .touched_files
            .insert(PathBuf::from("/repo/tests/a.rs"));
        state
            .touched_files
            .insert(PathBuf::from("/repo/tests/b.rs"));

        let panel = MetricsPanel::from_snapshot_at(&snapshot_with_state(state), now);

        assert_eq!(panel.tokens, "in 1200 / out 340  ↑ active");
        assert_eq!(panel.cpu, "█░░░░░░░   12%");
        assert_eq!(panel.memory, "342 MB");
        assert_eq!(panel.io, "↑1.2 KB/s");
        assert_eq!(panel.session, "6m 14s");
        assert_eq!(panel.tool_calls, 23);
        assert_eq!(panel.files_touched, 4);
        assert_eq!(panel.compacts, 2);
    }

    #[test]
    fn metrics_panel_marks_frozen_tokens() {
        let now = ts(12, 0, 0);
        let state = AgentState {
            session_started_at: Some(now - ChronoDuration::minutes(10)),
            last_token_activity_at: Some(now - ChronoDuration::seconds(252)),
            ..Default::default()
        };

        let panel = MetricsPanel::from_snapshot_at(&snapshot_with_state(state), now);

        assert_eq!(panel.tokens, "in 0 / out 0  ✗ frozen (4m 12s)");
    }
}
