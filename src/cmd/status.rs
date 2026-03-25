use crate::cmd::common::{
    collect_snapshot, display_path, home_dir, list_alive_session_selections, phase_label,
    prompt_for_session_selection, sessions_for_cwd, snapshot_json, status_exit_code, Snapshot,
};
use crate::cmd::config::{load_user_config, merge_timeout_secs, TimeoutSetting};
use anyhow::Result;
use clap::Args;
use saw_core::AgentPhase;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Args)]
pub struct StatusArgs {
    #[arg(long)]
    pub file: Option<PathBuf>,

    #[arg(long, default_value = ".")]
    pub dir: PathBuf,

    #[arg(long = "timeout", alias = "timeout-secs")]
    pub timeout: Option<TimeoutSetting>,

    #[arg(long)]
    pub session: Option<String>,

    #[arg(long)]
    pub all: bool,

    #[arg(long)]
    pub json: bool,

    #[arg(long, value_delimiter = ',')]
    pub guard: Vec<PathBuf>,
}

pub fn run(args: StatusArgs) -> Result<()> {
    let cwd = args.dir.canonicalize().unwrap_or_else(|_| args.dir.clone());
    let timeout_secs = merge_timeout_secs(args.timeout, &load_user_config()?);

    if args.all {
        let snapshots = collect_all_snapshots(&cwd, &args, timeout_secs)?;
        if args.json {
            let payload = snapshots.iter().map(snapshot_json).collect::<Vec<_>>();
            println!("{}", serde_json::to_string(&payload)?);
        } else {
            print_all_status_lines(&snapshots, io::stdout().lock())?;
        }

        let exit_code = snapshots
            .iter()
            .map(|snapshot| status_exit_code(&snapshot.phase))
            .max()
            .unwrap_or(0);
        std::process::exit(exit_code);
    }

    let snapshot = if args.file.is_none() && args.session.is_none() {
        let home = home_dir()?;
        let sessions = list_alive_session_selections(&home)?;
        let matching = sessions_for_cwd(&sessions, Some(&cwd));
        if matching.len() > 1 {
            let selection = prompt_for_session_selection(&matching, &cwd)?;
            match selection {
                Some(selection) => collect_snapshot(
                    Some(&selection.jsonl_path),
                    &selection.session.cwd_path(),
                    timeout_secs,
                    &args.guard,
                    Some(selection.session.session_id.as_str()),
                    false,
                )?,
                None => collect_snapshot(
                    args.file.as_ref(),
                    &cwd,
                    timeout_secs,
                    &args.guard,
                    args.session.as_deref(),
                    false,
                )?,
            }
        } else {
            collect_snapshot(
                args.file.as_ref(),
                &cwd,
                timeout_secs,
                &args.guard,
                args.session.as_deref(),
                false,
            )?
        }
    } else {
        collect_snapshot(
            args.file.as_ref(),
            &cwd,
            timeout_secs,
            &args.guard,
            args.session.as_deref(),
            false,
        )?
    };

    if args.json {
        println!("{}", serde_json::to_string(&snapshot_json(&snapshot))?);
    } else {
        print_status_line(&snapshot, io::stdout().lock())?;
    }

    std::process::exit(status_exit_code(&snapshot.phase));
}

fn collect_all_snapshots(
    cwd: &Path,
    args: &StatusArgs,
    timeout_secs: u64,
) -> Result<Vec<Snapshot>> {
    if let Some(file) = args.file.as_ref() {
        return Ok(vec![collect_snapshot(
            Some(file),
            cwd,
            timeout_secs,
            &args.guard,
            args.session.as_deref(),
            false,
        )?]);
    }

    let home = home_dir()?;
    let sessions = list_alive_session_selections(&home)?;
    let matching = if let Some(session_id) = args.session.as_deref() {
        sessions
            .into_iter()
            .filter(|selection| selection.session.session_id == session_id)
            .collect::<Vec<_>>()
    } else {
        sessions_for_cwd(&sessions, Some(cwd))
    };

    matching
        .into_iter()
        .map(|selection| {
            collect_snapshot(
                Some(&selection.jsonl_path),
                &selection.session.cwd_path(),
                timeout_secs,
                &args.guard,
                Some(selection.session.session_id.as_str()),
                false,
            )
        })
        .collect()
}

fn print_status_line(snapshot: &Snapshot, mut writer: impl Write) -> io::Result<()> {
    let phase = phase_label(&snapshot.phase);
    let bullet = status_glyph(&snapshot.phase);
    let last_file = snapshot
        .state
        .last_file_path
        .as_ref()
        .map(|path| display_path(&snapshot.cwd, path))
        .unwrap_or_else(|| "-".to_string());

    writeln!(
        writer,
        "{} {}  {} ({}s ago)  cpu: {:.1}%",
        bullet,
        phase,
        last_file,
        snapshot.silence.as_secs(),
        snapshot.state.latest_cpu_percent,
    )
}

fn print_all_status_lines(snapshots: &[Snapshot], mut writer: impl Write) -> io::Result<()> {
    for snapshot in snapshots {
        let phase = phase_label(&snapshot.phase);
        let bullet = status_glyph(&snapshot.phase);
        let last_file = snapshot
            .state
            .last_file_path
            .as_ref()
            .map(|path| display_path(&snapshot.cwd, path))
            .unwrap_or_else(|| "-".to_string());
        let session_id = snapshot.state.session_id.as_deref().unwrap_or("unknown");
        writeln!(
            writer,
            "{} {}  session={}  {} ({}s ago)  cpu: {:.1}%",
            bullet,
            phase,
            session_id,
            last_file,
            snapshot.silence.as_secs(),
            snapshot.state.latest_cpu_percent,
        )?;
    }

    Ok(())
}

fn status_glyph(phase: &AgentPhase) -> &'static str {
    match phase {
        AgentPhase::Working => "●",
        AgentPhase::Thinking => "◐",
        AgentPhase::ApiHang(_)
        | AgentPhase::ToolLoop { .. }
        | AgentPhase::TestLoop { .. }
        | AgentPhase::TaskBlocked { .. }
        | AgentPhase::ContextReset => "⚠",
        AgentPhase::ScopeLeaking { .. } => "✖",
        AgentPhase::Dead => "☠",
        AgentPhase::Idle(_) => "○",
        _ => "·",
    }
}

#[cfg(test)]
mod tests {
    use super::{print_all_status_lines, print_status_line, status_glyph};
    use crate::cmd::common::Snapshot;
    use saw_core::{AgentPhase, AgentState};
    use std::path::PathBuf;
    use std::time::Duration;

    #[test]
    fn maps_glyphs_for_key_phases() {
        assert_eq!(status_glyph(&AgentPhase::Working), "●");
        assert_eq!(status_glyph(&AgentPhase::Dead), "☠");
        assert_eq!(
            status_glyph(&AgentPhase::TaskBlocked {
                task_id: "3".into(),
                blocked_by: vec!["2".into()],
            }),
            "⚠"
        );
        assert_eq!(status_glyph(&AgentPhase::ContextReset), "⚠");
        assert_eq!(
            status_glyph(&AgentPhase::ScopeLeaking {
                violating_file: PathBuf::from("src/lib.rs"),
                guard_path: PathBuf::from("src/auth"),
            }),
            "✖"
        );
    }

    #[test]
    fn human_status_line_matches_expected_format() {
        let state = AgentState {
            last_file_path: Some(PathBuf::from("/repo/src/auth/login.rs")),
            latest_cpu_percent: 12.4,
            ..Default::default()
        };

        let snapshot = Snapshot {
            cwd: PathBuf::from("/repo"),
            session_file: None,
            pid: Some(42),
            state,
            phase: AgentPhase::Working,
            silence: Duration::from_secs(3),
        };

        let mut output = Vec::new();
        print_status_line(&snapshot, &mut output).unwrap();

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "● WORKING  src/auth/login.rs (3s ago)  cpu: 12.4%\n"
        );
    }

    #[test]
    fn all_status_lines_include_session_ids() {
        let first_state = AgentState {
            session_id: Some("ses-1".into()),
            last_file_path: Some(PathBuf::from("/repo/src/auth/login.rs")),
            latest_cpu_percent: 12.4,
            ..Default::default()
        };

        let second_state = AgentState {
            session_id: Some("ses-2".into()),
            last_file_path: Some(PathBuf::from("/repo/src/lib.rs")),
            latest_cpu_percent: 0.5,
            ..Default::default()
        };

        let snapshots = vec![
            Snapshot {
                cwd: PathBuf::from("/repo"),
                session_file: None,
                pid: Some(42),
                state: first_state,
                phase: AgentPhase::Working,
                silence: Duration::from_secs(3),
            },
            Snapshot {
                cwd: PathBuf::from("/repo"),
                session_file: None,
                pid: Some(43),
                state: second_state,
                phase: AgentPhase::Thinking,
                silence: Duration::from_secs(8),
            },
        ];

        let mut output = Vec::new();
        print_all_status_lines(&snapshots, &mut output).unwrap();
        let rendered = String::from_utf8(output).unwrap();

        assert!(rendered.contains("session=ses-1"));
        assert!(rendered.contains("session=ses-2"));
        assert!(rendered.contains("src/auth/login.rs"));
        assert!(rendered.contains("src/lib.rs"));
    }
}
