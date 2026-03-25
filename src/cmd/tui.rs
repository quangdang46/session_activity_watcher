use crate::cmd::common::{
    collect_snapshot, display_path, force_kill_pid, interrupt_pid, save_checkpoint, Snapshot,
};
use crate::cmd::config::{load_user_config, merge_timeout_secs, TimeoutSetting};
use crate::tui::widgets::{
    alerts_panel::AlertsPanel, file_activity::FileActivityPanel, metrics_panel::MetricsPanel,
    status_bar::StatusBar,
};
use anyhow::Result;
use clap::Args;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Terminal,
};
use saw_core::{compute_io_rate, AgentEvent, AgentPhase, ProcessMetrics};
use std::io::{self, Stdout};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

const ALERTS_PANEL_HEIGHT: u16 = 9;
const FILE_ACTIVITY_PANEL_MIN_HEIGHT: u16 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollPanel {
    FileActivity,
    Alerts,
}

#[derive(Debug, Args)]
pub struct TuiArgs {
    #[arg(long)]
    pub file: Option<PathBuf>,

    #[arg(long, default_value = ".")]
    pub dir: PathBuf,

    #[arg(long = "timeout", alias = "timeout-secs")]
    pub timeout: Option<TimeoutSetting>,

    #[arg(long, default_value_t = 500)]
    pub refresh: u64,

    #[arg(long, value_delimiter = ',')]
    pub guard: Vec<PathBuf>,
}

pub fn run(args: TuiArgs) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal, args);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

fn run_loop(terminal: &mut Terminal<CrosstermBackend<Stdout>>, args: TuiArgs) -> Result<()> {
    let TuiArgs {
        file,
        dir,
        timeout,
        refresh,
        guard,
    } = args;
    let timeout_secs = merge_timeout_secs(timeout, &load_user_config()?);
    let tick_rate = Duration::from_millis(refresh.max(50));
    let mut process_sampler = TuiProcessSampler::default();
    let mut current_guards = guard;
    let mut guard_input: Option<String> = None;
    let mut file_activity_scroll = 0u16;
    let mut alerts_scroll = 0u16;
    let mut active_scroll_panel = ScrollPanel::FileActivity;

    loop {
        let mut snapshot = collect_snapshot(
            file.as_ref(),
            &dir,
            timeout_secs,
            &current_guards,
            None,
            false,
        )?;
        process_sampler.refresh_snapshot(&mut snapshot);
        terminal.draw(|frame| {
            draw(
                frame,
                &snapshot,
                guard_input.as_deref(),
                file_activity_scroll,
                alerts_scroll,
                active_scroll_panel,
            )
        })?;

        if event::poll(tick_rate)? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                if let Some(buffer) = guard_input.as_mut() {
                    match key.code {
                        KeyCode::Enter => {
                            current_guards = parse_guard_input(buffer);
                            guard_input = None;
                        }
                        KeyCode::Esc => guard_input = None,
                        KeyCode::Backspace => {
                            buffer.pop();
                        }
                        KeyCode::Char(ch)
                            if !key.modifiers.contains(KeyModifiers::CONTROL)
                                && !key.modifiers.contains(KeyModifiers::ALT) =>
                        {
                            buffer.push(ch);
                        }
                        _ => {}
                    }
                    continue;
                }

                let max_file_activity_scroll = FileActivityPanel::from_snapshot(&snapshot)
                    .max_scroll(file_activity_panel_height(terminal.size()?));
                let max_alerts_scroll =
                    AlertsPanel::from_snapshot(&snapshot).max_scroll(ALERTS_PANEL_HEIGHT);
                match key.code {
                    KeyCode::Char('q') | KeyCode::Char('Q') => break,
                    KeyCode::Char('k') => {
                        if let Some(pid) = snapshot.pid {
                            let _ = interrupt_pid(pid);
                        }
                    }
                    KeyCode::Char('K') => {
                        if let Some(pid) = snapshot.pid {
                            let _ = force_kill_pid(pid);
                        }
                    }
                    KeyCode::Char('c') => {
                        let _ = save_checkpoint(&snapshot.state, &snapshot.cwd);
                    }
                    KeyCode::Char('g') => {
                        guard_input = Some(format_guard_input(&current_guards));
                    }
                    KeyCode::Tab => {
                        active_scroll_panel = match active_scroll_panel {
                            ScrollPanel::FileActivity => ScrollPanel::Alerts,
                            ScrollPanel::Alerts => ScrollPanel::FileActivity,
                        };
                    }
                    KeyCode::Up => match active_scroll_panel {
                        ScrollPanel::FileActivity => {
                            file_activity_scroll = file_activity_scroll.saturating_sub(1);
                        }
                        ScrollPanel::Alerts => {
                            alerts_scroll = alerts_scroll.saturating_sub(1);
                        }
                    },
                    KeyCode::Down => match active_scroll_panel {
                        ScrollPanel::FileActivity => {
                            file_activity_scroll = file_activity_scroll
                                .saturating_add(1)
                                .min(max_file_activity_scroll);
                        }
                        ScrollPanel::Alerts => {
                            alerts_scroll = alerts_scroll.saturating_add(1).min(max_alerts_scroll);
                        }
                    },
                    KeyCode::PageUp => match active_scroll_panel {
                        ScrollPanel::FileActivity => {
                            file_activity_scroll = file_activity_scroll.saturating_sub(5);
                        }
                        ScrollPanel::Alerts => {
                            alerts_scroll = alerts_scroll.saturating_sub(5);
                        }
                    },
                    KeyCode::PageDown => match active_scroll_panel {
                        ScrollPanel::FileActivity => {
                            file_activity_scroll = file_activity_scroll
                                .saturating_add(5)
                                .min(max_file_activity_scroll);
                        }
                        ScrollPanel::Alerts => {
                            alerts_scroll = alerts_scroll.saturating_add(5).min(max_alerts_scroll);
                        }
                    },
                    KeyCode::Home => match active_scroll_panel {
                        ScrollPanel::FileActivity => {
                            file_activity_scroll = 0;
                        }
                        ScrollPanel::Alerts => {
                            alerts_scroll = 0;
                        }
                    },
                    KeyCode::End => match active_scroll_panel {
                        ScrollPanel::FileActivity => {
                            file_activity_scroll = max_file_activity_scroll;
                        }
                        ScrollPanel::Alerts => {
                            alerts_scroll = max_alerts_scroll;
                        }
                    },
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

fn file_activity_panel_height(area: Rect) -> u16 {
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(FILE_ACTIVITY_PANEL_MIN_HEIGHT),
            Constraint::Length(4),
            Constraint::Length(ALERTS_PANEL_HEIGHT),
        ])
        .split(area)[1]
        .height
}

fn draw(
    frame: &mut ratatui::Frame<'_>,
    snapshot: &Snapshot,
    guard_input: Option<&str>,
    file_activity_scroll: u16,
    alerts_scroll: u16,
    active_scroll_panel: ScrollPanel,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(FILE_ACTIVITY_PANEL_MIN_HEIGHT),
            Constraint::Length(4),
            Constraint::Length(ALERTS_PANEL_HEIGHT),
        ])
        .split(frame.size());

    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(chunks[1]);

    frame.render_widget(StatusBar::from_snapshot(snapshot), chunks[0]);

    let file_activity = FileActivityPanel::from_snapshot(snapshot)
        .with_scroll(file_activity_scroll)
        .with_active(active_scroll_panel == ScrollPanel::FileActivity);
    let max_file_scroll = file_activity.max_scroll(top[0].height);
    frame.render_widget(
        file_activity.with_scroll(file_activity_scroll.min(max_file_scroll)),
        top[0],
    );

    frame.render_widget(MetricsPanel::from_snapshot(snapshot).render(), top[1]);
    render_guard_panel(frame, snapshot, chunks[2], guard_input.is_some());

    let alerts = AlertsPanel::from_snapshot(snapshot);
    let max_scroll = alerts.max_scroll(chunks[3].height);
    frame.render_widget(alerts.render(alerts_scroll.min(max_scroll)), chunks[3]);

    if let Some(buffer) = guard_input {
        render_guard_dialog(frame, snapshot, buffer);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GuardDisplay {
    guard: String,
    status: String,
    detail: String,
    has_guard: bool,
    has_violation: bool,
}

fn render_guard_panel(
    frame: &mut ratatui::Frame<'_>,
    snapshot: &Snapshot,
    area: Rect,
    editing: bool,
) {
    let guard = guard_display(snapshot);
    let title_style = if editing {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let first_line = if guard.status.is_empty() {
        Line::from(vec![Span::styled(
            guard.guard,
            if guard.has_guard {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            },
        )])
    } else {
        let status_style = if guard.has_violation {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        };
        Line::from(vec![
            Span::styled(guard.guard, Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("   "),
            Span::styled(guard.status, status_style),
        ])
    };
    let detail_style = if guard.has_violation {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let panel = Paragraph::new(vec![
        first_line,
        Line::from(Span::styled(guard.detail, detail_style)),
    ])
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(Span::styled("Guard", title_style)),
    );
    frame.render_widget(panel, area);
}

fn guard_display(snapshot: &Snapshot) -> GuardDisplay {
    if snapshot.state.guard_paths.is_empty() {
        return GuardDisplay {
            guard: "GUARD: none".to_string(),
            status: String::new(),
            detail: "Press [g] to set a guard path.".to_string(),
            has_guard: false,
            has_violation: false,
        };
    }

    let guard = format!("GUARD: {}", guard_summary(snapshot));
    if snapshot.state.scope_violation_count > 0 {
        let count = snapshot.state.scope_violation_count;
        return GuardDisplay {
            guard,
            status: format!(
                "⚠ {} {}",
                count,
                if count == 1 {
                    "violation"
                } else {
                    "violations"
                }
            ),
            detail: format!(
                "Last violation: {}",
                snapshot
                    .state
                    .latest_scope_violation
                    .as_ref()
                    .map(|path| display_path(&snapshot.cwd, path))
                    .unwrap_or_else(|| "-".to_string())
            ),
            has_guard: true,
            has_violation: true,
        };
    }

    GuardDisplay {
        guard,
        status: "✓ No violations".to_string(),
        detail: "Press [g] to update guard path.".to_string(),
        has_guard: true,
        has_violation: false,
    }
}

fn guard_summary(snapshot: &Snapshot) -> String {
    let Some(primary_guard) = active_guard_path(snapshot) else {
        return "none".to_string();
    };

    let mut summary = display_path(&snapshot.cwd, primary_guard);
    if snapshot.state.guard_paths.len() > 1 {
        summary.push_str(&format!(
            " (+{} more)",
            snapshot.state.guard_paths.len() - 1
        ));
    }
    summary
}

fn active_guard_path(snapshot: &Snapshot) -> Option<&Path> {
    match &snapshot.phase {
        AgentPhase::ScopeLeaking { guard_path, .. } => Some(guard_path.as_path()),
        _ => snapshot.state.guard_paths.first().map(PathBuf::as_path),
    }
}

fn format_guard_input(guards: &[PathBuf]) -> String {
    guards
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(",")
}

fn parse_guard_input(input: &str) -> Vec<PathBuf> {
    input
        .split(',')
        .map(str::trim)
        .filter(|segment| !segment.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn render_guard_dialog(frame: &mut ratatui::Frame<'_>, snapshot: &Snapshot, buffer: &str) {
    let area = centered_rect(80, 7, frame.size());
    let mut lines = vec![
        Line::from("Set guard path (comma-separated, empty to clear)"),
        Line::from(Span::styled(
            format!("> {buffer}"),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("Press Enter to save or Esc to cancel."),
    ];

    if snapshot.state.scope_violation_count > 0 {
        lines.push(Line::default());
        lines.push(Line::from(format!(
            "Selected detail: {}",
            snapshot
                .state
                .latest_scope_violation
                .as_ref()
                .map(|path| display_path(&snapshot.cwd, path))
                .unwrap_or_else(|| "-".to_string())
        )));
    }

    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default().borders(Borders::ALL).title(Span::styled(
                "Guard path",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
        ),
        area,
    );
}

fn centered_rect(width_percent: u16, height: u16, area: Rect) -> Rect {
    let popup_height = height.min(area.height.saturating_sub(2)).max(3);
    let vertical_margin = area.height.saturating_sub(popup_height) / 2;
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(vertical_margin),
            Constraint::Length(popup_height),
            Constraint::Min(0),
        ])
        .split(area);

    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1]);

    horizontal[1]
}

#[derive(Default)]
struct TuiProcessSampler {
    system: System,
    pid: Option<Pid>,
    previous_sample: Option<IoSample>,
}

#[derive(Clone, Copy)]
struct IoSample {
    captured_at: Instant,
    read_bytes: u64,
    write_bytes: u64,
}

impl TuiProcessSampler {
    fn refresh_snapshot(&mut self, snapshot: &mut Snapshot) {
        let Some(pid) = snapshot.pid else {
            self.pid = None;
            self.previous_sample = None;
            return;
        };

        let sys_pid = Pid::from_u32(pid);
        if self.pid != Some(sys_pid) {
            self.pid = Some(sys_pid);
            self.previous_sample = None;
            self.system = System::new();
        }

        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[sys_pid]),
            false,
            ProcessRefreshKind::nothing()
                .with_cpu()
                .with_memory()
                .with_disk_usage(),
        );

        let captured_at = Instant::now();
        let timestamp = chrono::Utc::now();
        let Some(process) = self.system.process(sys_pid) else {
            snapshot
                .state
                .apply(&AgentEvent::ProcessMetrics(ProcessMetrics {
                    timestamp,
                    process_alive: false,
                    cpu_percent: 0.0,
                    rss_bytes: 0,
                    virtual_bytes: 0,
                    io_read_bytes: 0,
                    io_write_bytes: 0,
                    io_read_rate: 0.0,
                    io_write_rate: 0.0,
                }));
            return;
        };

        let disk_usage = process.disk_usage();
        let io_read_bytes = disk_usage.total_read_bytes;
        let io_write_bytes = disk_usage.total_written_bytes;
        let (io_read_rate, io_write_rate) = self
            .previous_sample
            .map(|previous| {
                let elapsed = captured_at.saturating_duration_since(previous.captured_at);
                (
                    compute_io_rate(previous.read_bytes, io_read_bytes, elapsed),
                    compute_io_rate(previous.write_bytes, io_write_bytes, elapsed),
                )
            })
            .unwrap_or((0.0, 0.0));

        self.previous_sample = Some(IoSample {
            captured_at,
            read_bytes: io_read_bytes,
            write_bytes: io_write_bytes,
        });

        snapshot
            .state
            .apply(&AgentEvent::ProcessMetrics(ProcessMetrics {
                timestamp,
                process_alive: process.exists(),
                cpu_percent: process.cpu_usage(),
                rss_bytes: process.memory(),
                virtual_bytes: process.virtual_memory(),
                io_read_bytes,
                io_write_bytes,
                io_read_rate,
                io_write_rate,
            }));
    }
}

#[cfg(test)]
mod tests {
    use super::{guard_display, parse_guard_input, GuardDisplay};
    use crate::cmd::common::Snapshot;
    use saw_core::{AgentPhase, AgentState};
    use std::path::PathBuf;
    use std::time::Duration;

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

    #[test]
    fn guard_display_shows_empty_state() {
        let display = guard_display(&snapshot_with_state(
            AgentState::default(),
            AgentPhase::Working,
        ));

        assert_eq!(
            display,
            GuardDisplay {
                guard: "GUARD: none".into(),
                status: String::new(),
                detail: "Press [g] to set a guard path.".into(),
                has_guard: false,
                has_violation: false,
            }
        );
    }

    #[test]
    fn guard_display_shows_violation_count_and_last_file() {
        let state = AgentState {
            guard_paths: vec![PathBuf::from("/repo/src/auth")],
            scope_violation_count: 2,
            latest_scope_violation: Some(PathBuf::from("/repo/src/billing/mod.rs")),
            ..Default::default()
        };

        let display = guard_display(&snapshot_with_state(
            state,
            AgentPhase::ScopeLeaking {
                violating_file: PathBuf::from("/repo/src/billing/mod.rs"),
                guard_path: PathBuf::from("/repo/src/auth"),
            },
        ));

        assert_eq!(display.guard, "GUARD: src/auth");
        assert_eq!(display.status, "⚠ 2 violations");
        assert_eq!(display.detail, "Last violation: src/billing/mod.rs");
        assert!(display.has_guard);
        assert!(display.has_violation);
    }

    #[test]
    fn parse_guard_input_accepts_multiple_paths() {
        assert_eq!(
            parse_guard_input("src/auth, tests/auth , /tmp/guard"),
            vec![
                PathBuf::from("src/auth"),
                PathBuf::from("tests/auth"),
                PathBuf::from("/tmp/guard"),
            ]
        );
        assert!(parse_guard_input("  ,  ").is_empty());
    }
}
