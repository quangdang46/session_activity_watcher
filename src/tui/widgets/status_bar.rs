use crate::cmd::common::{phase_label, Snapshot};
use chrono::{DateTime, Utc};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};
use saw_core::AgentPhase;
use std::{borrow::Cow, time::Duration};

const DEFAULT_AGENT_NAME: &str = "Claude";
const KEYBINDINGS: &str = "[q]uit [k]ill [?]help";

pub struct StatusBar<'a> {
    agent_name: Cow<'a, str>,
    pid: Option<u32>,
    phase_label: &'static str,
    phase_style: Style,
    elapsed: Duration,
}

impl<'a> StatusBar<'a> {
    pub fn from_snapshot(snapshot: &'a Snapshot) -> Self {
        Self::from_snapshot_at(snapshot, Utc::now())
    }

    fn from_snapshot_at(snapshot: &'a Snapshot, now: DateTime<Utc>) -> Self {
        let elapsed = snapshot
            .state
            .session_started_at
            .and_then(|started_at| now.signed_duration_since(started_at).to_std().ok())
            .unwrap_or_default();

        Self {
            agent_name: agent_name(snapshot),
            pid: snapshot.pid,
            phase_label: phase_label(&snapshot.phase),
            phase_style: phase_style(&snapshot.phase),
            elapsed,
        }
    }
}

impl Widget for StatusBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let block = Block::default().borders(Borders::ALL).title("Status");
        let inner = block.inner(area);
        block.render(area, buf);

        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let agent_text = format!(
            "{} ({})",
            self.agent_name,
            self.pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "-".to_string())
        );
        let phase_text = format!(" {}", self.phase_label);
        let elapsed_text = format!(" {}", format_elapsed(self.elapsed));
        let keybindings_text = format!(" {}", KEYBINDINGS);

        let mut remaining = inner.width;
        let keybindings_width = (keybindings_text.chars().count() as u16).min(remaining);
        remaining = remaining.saturating_sub(keybindings_width);

        let elapsed_width = (elapsed_text.chars().count() as u16).min(remaining);
        remaining = remaining.saturating_sub(elapsed_width);

        let phase_width = (phase_text.chars().count() as u16).min(remaining);
        let agent_width = remaining.saturating_sub(phase_width);

        let segments = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(agent_width),
                Constraint::Length(phase_width),
                Constraint::Length(elapsed_width),
                Constraint::Length(keybindings_width),
            ])
            .split(inner);

        render_segment(
            segments[0],
            &agent_text,
            Style::default().add_modifier(Modifier::BOLD),
            buf,
        );
        render_segment(segments[1], &phase_text, self.phase_style, buf);
        render_segment(
            segments[2],
            &elapsed_text,
            Style::default().fg(Color::DarkGray),
            buf,
        );
        render_segment(
            segments[3],
            &keybindings_text,
            Style::default().fg(Color::Cyan),
            buf,
        );
    }
}

fn agent_name(_snapshot: &Snapshot) -> Cow<'_, str> {
    Cow::Borrowed(DEFAULT_AGENT_NAME)
}

fn render_segment(area: Rect, text: &str, style: Style, buf: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    Paragraph::new(Line::from(Span::styled(
        truncate_text(text, area.width as usize),
        style,
    )))
    .render(area, buf);
}

fn phase_style(phase: &AgentPhase) -> Style {
    match phase {
        AgentPhase::Working => Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD),
        AgentPhase::Thinking
        | AgentPhase::TaskBlocked {
            task_id: _,
            blocked_by: _,
        } => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        AgentPhase::ApiHang(_)
        | AgentPhase::ToolLoop { .. }
        | AgentPhase::TestLoop { .. }
        | AgentPhase::ScopeLeaking { .. }
        | AgentPhase::Dead => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        AgentPhase::ContextReset => Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
        AgentPhase::Idle(_) => Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
        AgentPhase::Initializing => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    }
}

fn format_elapsed(duration: Duration) -> String {
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

fn truncate_text(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }

    let char_count = text.chars().count();
    if char_count <= width {
        return text.to_string();
    }

    if width == 1 {
        return "…".to_string();
    }

    let mut truncated = text.chars().take(width - 1).collect::<String>();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::{format_elapsed, phase_style, truncate_text, StatusBar};
    use crate::cmd::common::Snapshot;
    use chrono::{Duration as ChronoDuration, TimeZone, Utc};
    use ratatui::{buffer::Buffer, layout::Rect, style::Color, widgets::Widget};
    use saw_core::{AgentPhase, AgentState};
    use std::{path::PathBuf, time::Duration};

    fn ts(hour: u32, minute: u32, second: u32) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 24, hour, minute, second)
            .single()
            .expect("valid timestamp")
    }

    fn snapshot_with_state(state: AgentState, phase: AgentPhase) -> Snapshot {
        Snapshot {
            cwd: PathBuf::from("/repo"),
            session_file: None,
            pid: Some(42),
            state,
            phase,
            silence: Duration::from_secs(0),
        }
    }

    fn buffer_line(buf: &Buffer, y: u16) -> String {
        let mut line = String::new();
        for x in 0..buf.area.width {
            line.push_str(buf.get(x, y).symbol());
        }
        line
    }

    #[test]
    fn formats_elapsed_human_readably() {
        assert_eq!(format_elapsed(Duration::from_secs(14)), "14s");
        assert_eq!(format_elapsed(Duration::from_secs(332)), "5m 32s");
        assert_eq!(format_elapsed(Duration::from_secs(7_560)), "2h 6m");
    }

    #[test]
    fn phase_colors_match_spec() {
        assert_eq!(phase_style(&AgentPhase::Working).fg, Some(Color::Green));
        assert_eq!(phase_style(&AgentPhase::Thinking).fg, Some(Color::Yellow));
        assert_eq!(
            phase_style(&AgentPhase::ApiHang(Duration::from_secs(1))).fg,
            Some(Color::Red)
        );
        assert_eq!(phase_style(&AgentPhase::Initializing).fg, Some(Color::Cyan));
    }

    #[test]
    fn truncates_text_for_narrow_widths() {
        assert_eq!(truncate_text("Claude (42)", 20), "Claude (42)");
        assert_eq!(truncate_text("Claude (42)", 6), "Claud…");
        assert_eq!(truncate_text("Claude (42)", 1), "…");
        assert_eq!(truncate_text("Claude (42)", 0), "");
    }

    #[test]
    fn status_bar_renders_expected_segments() {
        let now = ts(12, 0, 0);
        let state = AgentState {
            session_started_at: Some(now - ChronoDuration::seconds(332)),
            ..Default::default()
        };
        let snapshot = snapshot_with_state(state, AgentPhase::Working);

        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 3));
        StatusBar::from_snapshot_at(&snapshot, now).render(Rect::new(0, 0, 80, 3), &mut buf);

        let line = buffer_line(&buf, 1);
        assert!(line.contains("Claude (42)"));
        assert!(line.contains("WORKING"));
        assert!(line.contains("5m 32s"));
        assert!(line.contains("[q]uit [k]ill [?]help"));
    }

    #[test]
    fn status_bar_keeps_keybindings_visible_on_narrow_term() {
        let now = ts(12, 0, 0);
        let state = AgentState {
            session_started_at: Some(now - ChronoDuration::seconds(332)),
            ..Default::default()
        };
        let snapshot = snapshot_with_state(state, AgentPhase::Thinking);

        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 3));
        StatusBar::from_snapshot_at(&snapshot, now).render(Rect::new(0, 0, 40, 3), &mut buf);

        let line = buffer_line(&buf, 1);
        assert!(line.contains("[q]uit"));
        assert!(line.contains("[k]ill"));
        assert!(line.contains("[?]help"));
    }
}
