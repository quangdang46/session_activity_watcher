use crate::cmd::common::{display_path, Snapshot};
use chrono::{DateTime, Utc};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Widget},
};
use saw_core::{FileChangeKind, ToolLineChange};
use std::{collections::HashMap, path::PathBuf};

const MAX_FILE_ACTIVITY_ITEMS: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileActivityEntry {
    timestamp: DateTime<Utc>,
    path: String,
    path_key: PathBuf,
    kind: FileChangeKind,
    line_change: Option<ToolLineChange>,
    repeat_index: u32,
}

pub struct FileActivityPanel {
    entries: Vec<FileActivityEntry>,
    now: DateTime<Utc>,
    scroll: u16,
    active: bool,
}

impl FileActivityPanel {
    pub fn from_snapshot(snapshot: &Snapshot) -> Self {
        Self::from_snapshot_at(snapshot, Utc::now())
    }

    fn from_snapshot_at(snapshot: &Snapshot, now: DateTime<Utc>) -> Self {
        let mut entries = Vec::new();
        for event in snapshot.state.recently_modified_files.iter().rev() {
            if let Some(previous) = entries.last_mut() {
                if should_merge_adjacent(previous, event) {
                    if previous.line_change.is_none() {
                        previous.line_change = event.line_change.clone();
                    }
                    previous.kind = merge_kind(previous.kind.clone(), event.kind.clone());
                    continue;
                }
            }

            entries.push(FileActivityEntry {
                timestamp: event.timestamp,
                path: display_path(&snapshot.cwd, &event.path),
                path_key: event.path.clone(),
                kind: event.kind.clone(),
                line_change: event.line_change.clone(),
                repeat_index: 0,
            });
            if entries.len() == MAX_FILE_ACTIVITY_ITEMS {
                break;
            }
        }

        let mut repeat_counts = HashMap::<PathBuf, u32>::new();
        for entry in &mut entries {
            let count = repeat_counts.entry(entry.path_key.clone()).or_default();
            *count += 1;
            entry.repeat_index = *count;
        }

        Self {
            entries,
            now,
            scroll: 0,
            active: false,
        }
    }

    pub fn with_scroll(mut self, scroll: u16) -> Self {
        self.scroll = scroll;
        self
    }

    pub fn with_active(mut self, active: bool) -> Self {
        self.active = active;
        self
    }

    pub fn content_height(&self) -> usize {
        self.entries.len().max(1)
    }

    pub fn max_scroll(&self, area_height: u16) -> u16 {
        let visible_height = area_height.saturating_sub(2) as usize;
        self.content_height().saturating_sub(visible_height) as u16
    }
}

impl Widget for FileActivityPanel {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let title_style = if self.active {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::BOLD)
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled("File activity", title_style));
        let width = block.inner(area).width as usize;
        let lines = if self.entries.is_empty() {
            vec![Line::from(Span::styled(
                "No recent file activity",
                Style::default().fg(Color::DarkGray),
            ))]
        } else {
            self.entries
                .iter()
                .map(|entry| build_line(entry, self.now, width))
                .collect()
        };

        Paragraph::new(lines)
            .scroll((self.scroll, 0))
            .block(block)
            .render(area, buf);
    }
}

fn should_merge_adjacent(
    previous: &FileActivityEntry,
    current: &saw_core::FileModification,
) -> bool {
    previous.path_key == current.path && previous.kind == current.kind
}

fn merge_kind(existing: FileChangeKind, incoming: FileChangeKind) -> FileChangeKind {
    match (existing, incoming) {
        (FileChangeKind::Deleted, _) | (_, FileChangeKind::Deleted) => FileChangeKind::Deleted,
        (FileChangeKind::Created, _) | (_, FileChangeKind::Created) => FileChangeKind::Created,
        (FileChangeKind::Modified, _) | (_, FileChangeKind::Modified) => FileChangeKind::Modified,
        _ => FileChangeKind::Read,
    }
}

fn build_line(entry: &FileActivityEntry, now: DateTime<Utc>, width: usize) -> Line<'static> {
    let age = format_age(now, entry.timestamp);
    let age_cell = format!("{age:>5}");
    let icon = icon(entry.kind.clone());
    let line_change = format_line_change(entry.line_change.as_ref());
    let loop_marker = if entry.repeat_index > 1 {
        Some(format!("({})", entry.repeat_index))
    } else {
        None
    };

    let prefix_width = age_cell.chars().count() + 2 + icon.chars().count() + 2;
    let suffix_width = line_change
        .as_ref()
        .map(|text| text.chars().count() + 2)
        .unwrap_or(0)
        + loop_marker
            .as_ref()
            .map(|text| text.chars().count() + 2)
            .unwrap_or(0);
    let path_width = width.saturating_sub(prefix_width + suffix_width).max(1);
    let path = truncate_path(&entry.path, path_width);

    let mut spans = vec![
        Span::styled(age_cell, Style::default().fg(Color::DarkGray)),
        Span::raw("  "),
        Span::styled(icon.to_string(), icon_style(entry.kind.clone())),
        Span::raw("  "),
        Span::raw(path),
    ];

    if let Some(line_change) = line_change {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            line_change,
            line_change_style(entry.line_change.as_ref()),
        ));
    }

    if let Some(loop_marker) = loop_marker {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            loop_marker,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    Line::from(spans)
}

fn format_age(now: DateTime<Utc>, timestamp: DateTime<Utc>) -> String {
    let elapsed = now
        .signed_duration_since(timestamp)
        .to_std()
        .unwrap_or_default();
    let total_secs = elapsed.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;

    if hours > 0 {
        if minutes > 0 {
            format!("{hours}h{minutes}m")
        } else {
            format!("{hours}h")
        }
    } else if minutes > 0 {
        if minutes < 10 && seconds > 0 {
            format!("{minutes}m{seconds}s")
        } else {
            format!("{minutes}m")
        }
    } else {
        format!("{seconds}s")
    }
}

fn icon(kind: FileChangeKind) -> &'static str {
    match kind {
        FileChangeKind::Read => "○",
        FileChangeKind::Deleted => "✗",
        FileChangeKind::Created | FileChangeKind::Modified => "✎",
    }
}

fn icon_style(kind: FileChangeKind) -> Style {
    match kind {
        FileChangeKind::Read => Style::default().fg(Color::DarkGray),
        FileChangeKind::Deleted => Style::default().fg(Color::Red),
        FileChangeKind::Created => Style::default().fg(Color::Green),
        FileChangeKind::Modified => Style::default().fg(Color::Cyan),
    }
}

fn format_line_change(line_change: Option<&ToolLineChange>) -> Option<String> {
    let line_change = line_change?;
    match (line_change.added, line_change.removed) {
        (0, 0) => None,
        (added, 0) => Some(format!("+{added}L")),
        (0, removed) => Some(format!("-{removed}L")),
        (added, removed) => Some(format!("+{added}L -{removed}L")),
    }
}

fn line_change_style(line_change: Option<&ToolLineChange>) -> Style {
    match line_change {
        Some(ToolLineChange { added, removed }) if *removed > 0 && *added == 0 => {
            Style::default().fg(Color::Red)
        }
        Some(ToolLineChange { added, removed }) if *added > 0 && *removed == 0 => {
            Style::default().fg(Color::Green)
        }
        Some(_) => Style::default().fg(Color::Yellow),
        None => Style::default(),
    }
}

fn truncate_path(path: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }

    let path_len = path.chars().count();
    if path_len <= width {
        return path.to_string();
    }

    if width == 1 {
        return "…".to_string();
    }

    let Some((parent, filename)) = path.rsplit_once('/') else {
        return format!("…{}", take_last_chars(path, width.saturating_sub(1)));
    };
    let filename_len = filename.chars().count();
    if filename_len + 2 >= width {
        return format!("…{}", take_last_chars(filename, width.saturating_sub(1)));
    }

    let tail_budget = width.saturating_sub(filename_len + 3);
    if tail_budget == 0 {
        return format!("…/{filename}");
    }

    let parent_tail = take_last_chars(parent, tail_budget);
    let parent_tail = parent_tail
        .trim_start_matches(|ch| ch != '/')
        .trim_start_matches('/');

    if parent_tail.is_empty() {
        format!("…/{filename}")
    } else {
        format!("…/{parent_tail}/{filename}")
    }
}

fn take_last_chars(text: &str, count: usize) -> String {
    text.chars()
        .rev()
        .take(count)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{format_age, format_line_change, truncate_path, FileActivityPanel};
    use crate::cmd::common::Snapshot;
    use chrono::{TimeZone, Utc};
    use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};
    use saw_core::{AgentPhase, AgentState, FileChangeKind, FileModification, ToolLineChange};
    use std::{path::PathBuf, time::Duration};

    fn ts(hour: u32, minute: u32, second: u32) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 24, hour, minute, second)
            .single()
            .expect("valid timestamp")
    }

    fn snapshot_with_files(files: Vec<FileModification>) -> Snapshot {
        let state = AgentState {
            recently_modified_files: files.into(),
            ..Default::default()
        };
        Snapshot {
            cwd: PathBuf::from("/repo"),
            session_file: None,
            pid: Some(42),
            phase: AgentPhase::Working,
            silence: Duration::from_secs(0),
            state,
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
    fn formats_compact_ages() {
        assert_eq!(format_age(ts(12, 0, 10), ts(12, 0, 8)), "2s");
        assert_eq!(format_age(ts(12, 1, 0), ts(12, 0, 0)), "1m");
        assert_eq!(format_age(ts(12, 4, 12), ts(12, 0, 0)), "4m12s");
        assert_eq!(format_age(ts(14, 0, 0), ts(12, 30, 0)), "1h30m");
    }

    #[test]
    fn formats_line_changes() {
        assert_eq!(
            format_line_change(Some(&ToolLineChange {
                added: 47,
                removed: 0,
            })),
            Some("+47L".to_string())
        );
        assert_eq!(
            format_line_change(Some(&ToolLineChange {
                added: 0,
                removed: 3,
            })),
            Some("-3L".to_string())
        );
        assert_eq!(
            format_line_change(Some(&ToolLineChange {
                added: 4,
                removed: 2,
            })),
            Some("+4L -2L".to_string())
        );
    }

    #[test]
    fn truncates_long_paths_and_keeps_filename_visible() {
        let path = "src/very/long/nested/components/file_activity.rs";
        let truncated = truncate_path(path, 24);

        assert!(truncated.starts_with("…/"));
        assert!(truncated.ends_with("file_activity.rs"));
        assert!(truncated.chars().count() <= 24);
    }

    #[test]
    fn renders_icons_loop_counts_and_line_changes() {
        let now = ts(12, 0, 0);
        let snapshot = snapshot_with_files(vec![
            FileModification {
                timestamp: ts(11, 59, 0),
                path: PathBuf::from("/repo/src/auth/login.rs"),
                kind: FileChangeKind::Modified,
                line_change: Some(ToolLineChange {
                    added: 3,
                    removed: 0,
                }),
            },
            FileModification {
                timestamp: ts(11, 59, 26),
                path: PathBuf::from("/repo/src/utils/crypto.rs"),
                kind: FileChangeKind::Read,
                line_change: None,
            },
            FileModification {
                timestamp: ts(11, 59, 40),
                path: PathBuf::from("/repo/src/old.rs"),
                kind: FileChangeKind::Deleted,
                line_change: Some(ToolLineChange {
                    added: 0,
                    removed: 3,
                }),
            },
            FileModification {
                timestamp: ts(11, 59, 58),
                path: PathBuf::from("/repo/src/auth/login.rs"),
                kind: FileChangeKind::Modified,
                line_change: Some(ToolLineChange {
                    added: 47,
                    removed: 0,
                }),
            },
        ]);

        let panel = FileActivityPanel::from_snapshot_at(&snapshot, now);
        let mut buf = Buffer::empty(Rect::new(0, 0, 60, 8));
        panel.render(Rect::new(0, 0, 60, 8), &mut buf);

        let rendered = (1..7).map(|y| buffer_line(&buf, y)).collect::<Vec<_>>();
        assert!(rendered
            .iter()
            .any(|line| line.contains("2s  ✎  src/auth/login.rs  +47L")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("20s  ✗  src/old.rs  -3L")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("34s  ○  src/utils/crypto.rs")));
        assert!(rendered
            .iter()
            .any(|line| line.contains("1m  ✎  src/auth/login.rs  +3L  (2)")));
    }

    #[test]
    fn merges_adjacent_duplicate_events_from_tool_and_watcher() {
        let now = ts(12, 0, 0);
        let snapshot = snapshot_with_files(vec![
            FileModification {
                timestamp: ts(11, 59, 58),
                path: PathBuf::from("/repo/src/auth/login.rs"),
                kind: FileChangeKind::Modified,
                line_change: Some(ToolLineChange {
                    added: 47,
                    removed: 0,
                }),
            },
            FileModification {
                timestamp: ts(11, 59, 58),
                path: PathBuf::from("/repo/src/auth/login.rs"),
                kind: FileChangeKind::Modified,
                line_change: None,
            },
        ]);

        let panel = FileActivityPanel::from_snapshot_at(&snapshot, now);
        assert_eq!(panel.content_height(), 1);
        let mut buf = Buffer::empty(Rect::new(0, 0, 60, 4));
        panel.render(Rect::new(0, 0, 60, 4), &mut buf);
        let line = buffer_line(&buf, 1);
        assert!(line.contains("src/auth/login.rs"));
        assert!(line.contains("+47L"));
        assert!(!line.contains("(2)"));
    }

    #[test]
    fn max_scroll_uses_twenty_item_cap() {
        let snapshot = snapshot_with_files(
            (0..25)
                .map(|index| FileModification {
                    timestamp: ts(12, 0, 0),
                    path: PathBuf::from(format!("/repo/src/file-{index}.rs")),
                    kind: FileChangeKind::Modified,
                    line_change: None,
                })
                .collect(),
        );
        let panel = FileActivityPanel::from_snapshot_at(&snapshot, ts(12, 0, 1));

        assert_eq!(panel.content_height(), 20);
        assert_eq!(panel.max_scroll(5), 17);
    }

    #[test]
    fn scroll_shows_older_entries() {
        let snapshot = snapshot_with_files(
            (0..6)
                .map(|index| FileModification {
                    timestamp: ts(12, 0, index),
                    path: PathBuf::from(format!("/repo/src/file-{index}.rs")),
                    kind: FileChangeKind::Modified,
                    line_change: None,
                })
                .collect(),
        );
        let panel = FileActivityPanel::from_snapshot_at(&snapshot, ts(12, 0, 6)).with_scroll(2);
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 5));
        panel.render(Rect::new(0, 0, 40, 5), &mut buf);

        let lines = [
            buffer_line(&buf, 1),
            buffer_line(&buf, 2),
            buffer_line(&buf, 3),
        ];
        assert!(lines.iter().any(|line| line.contains("src/file-3.rs")));
        assert!(!lines.iter().any(|line| line.contains("src/file-5.rs")));
    }
}
