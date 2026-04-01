use crate::cmd::common::{
    display_path, force_kill_pid, home_dir, interrupt_pid, phase_label, refresh_task_context,
    save_checkpoint,
};
use crate::cmd::config::{
    load_user_config, merge_on_stuck_action, merge_timeout_secs, TimeoutSetting,
};
use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use clap::{Args, ValueEnum};
use saw_core::{compute_silence, AgentPhase, AgentState, ClassifierConfig};
use saw_daemon::{
    AlertActionExecutor, AlertContext, AlertNotification, Alerter, AlerterConfig,
    RuntimeStateRefresher, ScopeLeakAction as DaemonScopeLeakAction,
    StuckAction as DaemonStuckAction, WatcherRuntime, WatcherRuntimeOptions, WatcherRuntimeTarget,
    DEFAULT_ALERT_RATE_LIMIT,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Builder;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum StuckAction {
    #[default]
    Warn,
    Bell,
    Kill,
    CheckpointKill,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScopeLeakAction {
    Warn,
    Bell,
    Kill,
}

#[derive(Debug, Args)]
pub struct WatchArgs {
    #[arg(long, hide = true)]
    pub file: Option<PathBuf>,

    #[arg(long)]
    pub pid: Option<u32>,

    #[arg(long)]
    pub session: Option<String>,

    #[arg(long, default_value = ".")]
    pub dir: PathBuf,

    #[arg(long = "timeout", alias = "timeout-secs")]
    pub timeout: Option<TimeoutSetting>,

    #[arg(long, value_delimiter = ',')]
    pub guard: Vec<PathBuf>,

    #[arg(long, value_enum)]
    pub on_stuck: Option<StuckAction>,

    #[arg(long)]
    pub checkpoint: bool,

    #[arg(long, value_enum, default_value_t = ScopeLeakAction::Warn)]
    pub on_scope_leak: ScopeLeakAction,

    #[arg(long)]
    pub robot: bool,

    #[arg(long)]
    pub quiet: bool,

    #[arg(long)]
    pub no_color: bool,

    #[arg(long)]
    pub force_poll: bool,
}

type WatchTarget = WatcherRuntimeTarget;

struct WatchStateRefresher;

impl RuntimeStateRefresher for WatchStateRefresher {
    fn refresh(&self, state: &mut AgentState, _cwd: &Path) -> Result<()> {
        refresh_task_context(&home_dir()?, state)
    }
}

pub fn run(args: WatchArgs) -> Result<()> {
    Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to create tokio runtime for watch command")?
        .block_on(run_async(args))
}

fn resolve_runtime_settings(args: &WatchArgs) -> Result<(u64, StuckAction)> {
    let config = load_user_config()?;
    Ok((
        merge_timeout_secs(args.timeout, &config),
        merge_on_stuck_action(args.on_stuck, &config),
    ))
}

async fn run_async(args: WatchArgs) -> Result<()> {
    let (timeout_secs, on_stuck) = resolve_runtime_settings(&args)?;
    let classifier_config = ClassifierConfig {
        thinking_after: Duration::from_secs(45),
        api_hang_after: Duration::from_secs(timeout_secs),
        idle_after: Duration::from_secs(600),
        ..ClassifierConfig::default()
    };
    let refresher = Arc::new(WatchStateRefresher);
    let mut runtime = WatcherRuntime::attach(
        WatcherRuntimeOptions {
            home_dir: home_dir()?,
            cwd: args.dir.clone(),
            file: args.file.clone(),
            pid: args.pid,
            session_id: args.session.clone(),
            guard_paths: args.guard.clone(),
            force_poll: args.force_poll,
            classifier_config,
        },
        refresher,
    )?;
    let target = runtime.target().clone();
    let initial = runtime.initial_update();
    let mut state = initial.state.clone();
    let mut alerter = Alerter::new(AlerterConfig {
        stuck_action: map_stuck_action(on_stuck),
        scope_leak_action: map_scope_leak_action(args.on_scope_leak),
        checkpoint_before_action: args.checkpoint,
        quiet: args.quiet,
        color: !args.no_color,
        rate_limit: DEFAULT_ALERT_RATE_LIMIT,
    });
    let mut executor = WatchActionExecutor;
    let initial_timestamp = initial.timestamp;
    let current_phase = initial.phase.clone();
    let initial_alert = alerter.on_phase_change(
        None,
        &current_phase,
        AlertContext {
            timestamp: initial_timestamp,
            cwd: &target.cwd,
            state: &state,
            pid: target.pid,
        },
        &mut executor,
    )?;

    emit_status(
        None,
        &current_phase,
        &state,
        &target,
        &args,
        initial_timestamp,
        &alerter,
        initial_alert.as_ref(),
    )?;
    if let Some(alert) = initial_alert.as_ref() {
        emit_alert_output(alert, args.robot);
    }

    if matches!(current_phase, AgentPhase::Dead) {
        runtime.shutdown().await;
        return Ok(());
    }

    let mut previous_phase = Some(current_phase);
    let result = loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break Ok(()),
            update = runtime.next_update() => match update? {
                Some(update) => {
                    let timestamp = update.timestamp;
                    state = update.state.clone();
                    let phase = update.phase.clone();
                    let alert = alerter.on_phase_change(
                        previous_phase.as_ref(),
                        &phase,
                        AlertContext {
                            timestamp,
                            cwd: &target.cwd,
                            state: &state,
                            pid: target.pid,
                        },
                        &mut executor,
                    )?;
                    emit_status(
                        previous_phase.as_ref(),
                        &phase,
                        &state,
                        &target,
                        &args,
                        timestamp,
                        &alerter,
                        alert.as_ref(),
                    )?;
                    if let Some(alert) = alert.as_ref() {
                        emit_alert_output(alert, args.robot);
                    }

                    if matches!(phase, AgentPhase::Dead) {
                        break Ok(());
                    }

                    previous_phase = Some(phase);
                }
                None => break Ok(()),
            }
        }
    };

    runtime.shutdown().await;
    result
}

#[cfg(test)]
fn resolve_watch_target(args: &WatchArgs) -> Result<WatchTarget> {
    saw_daemon::resolve_watch_target(&WatcherRuntimeOptions {
        home_dir: home_dir()?,
        cwd: args.dir.clone(),
        file: args.file.clone(),
        pid: args.pid,
        session_id: args.session.clone(),
        guard_paths: args.guard.clone(),
        force_poll: args.force_poll,
        classifier_config: ClassifierConfig {
            thinking_after: Duration::from_secs(45),
            api_hang_after: Duration::from_secs(merge_timeout_secs(
                args.timeout,
                &load_user_config()?,
            )),
            idle_after: Duration::from_secs(600),
            ..ClassifierConfig::default()
        },
    })
}

#[allow(clippy::too_many_arguments)]
fn emit_status(
    previous_phase: Option<&AgentPhase>,
    phase: &AgentPhase,
    state: &AgentState,
    target: &WatchTarget,
    args: &WatchArgs,
    timestamp: DateTime<Utc>,
    alerter: &Alerter,
    alert: Option<&AlertNotification>,
) -> Result<()> {
    if args.robot && alert.is_some() {
        return Ok(());
    }

    if !alerter.should_emit_status(alert) {
        return Ok(());
    }

    if args.robot {
        println!(
            "{}",
            serde_json::to_string(&status_payload(
                previous_phase,
                phase,
                state,
                target,
                timestamp,
                alert,
            ))?
        );
    } else {
        print_human_status(previous_phase, phase, state, target, args, timestamp, alert);
    }
    std::io::stdout().flush()?;

    Ok(())
}

fn status_payload(
    previous_phase: Option<&AgentPhase>,
    phase: &AgentPhase,
    state: &AgentState,
    target: &WatchTarget,
    timestamp: DateTime<Utc>,
    alert: Option<&AlertNotification>,
) -> Value {
    let phase_changed = phase_kind_changed(previous_phase, phase);
    json!({
        "event": if phase_changed { "phase_change" } else { "status" },
        "timestamp": format_timestamp(timestamp),
        "phase": phase_label(phase),
        "previous_phase": previous_phase.map(phase_label),
        "alert": alert.is_some(),
        "alert_message": alert.map(|value| value.message.as_str()),
        "alert_action": alert.map(|value| value.action),
        "pid": target.pid,
        "session_id": state.session_id.as_ref().or(target.session_id.as_ref()),
        "file": state.last_file_path.as_ref().map(|path| display_path(&target.cwd, path)),
        "silence_secs": compute_silence(state, timestamp).as_secs(),
        "cpu_percent": state.latest_cpu_percent,
        "source": target.jsonl_path.display().to_string(),
        "suggestion": alert.and_then(|value| value.suggestion),
        "details": phase_details(phase, state, &target.cwd, timestamp),
    })
}

fn print_human_status(
    previous_phase: Option<&AgentPhase>,
    phase: &AgentPhase,
    state: &AgentState,
    target: &WatchTarget,
    args: &WatchArgs,
    timestamp: DateTime<Utc>,
    alert: Option<&AlertNotification>,
) {
    if alert.is_some() {
        return;
    }

    let phase_name = phase_label(phase);
    let phase_display = if args.no_color {
        phase_name.to_string()
    } else {
        colorize_phase(phase, phase_name)
    };
    let prefix = if phase_kind_changed(previous_phase, phase) {
        "PHASE"
    } else {
        "STATUS"
    };
    let from = previous_phase
        .filter(|previous| *previous != phase)
        .map(|previous| format!(" from={}", phase_label(previous)))
        .unwrap_or_default();

    println!(
        "{} {} {}{} pid={} file={} silence={}s cpu={:.1}% session={} source={}",
        format_timestamp(timestamp),
        prefix,
        phase_display,
        from,
        target
            .pid
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "-".to_string()),
        state
            .last_file_path
            .as_ref()
            .map(|path| display_path(&target.cwd, path))
            .unwrap_or_else(|| "-".to_string()),
        compute_silence(state, timestamp).as_secs(),
        state.latest_cpu_percent,
        state
            .session_id
            .as_deref()
            .or(target.session_id.as_deref())
            .unwrap_or("unknown"),
        target.jsonl_path.display(),
    );
}

fn emit_alert_output(alert: &AlertNotification, robot: bool) {
    if robot {
        let payload = json!({
            "event": "alert",
            "timestamp": format_timestamp(alert.timestamp),
            "phase": alert.phase,
            "previous_phase": alert.previous_phase,
            "message": &alert.message,
            "suggestion": alert.suggestion,
            "action": alert.action,
            "checkpoint": alert
                .checkpoint_dir
                .as_ref()
                .map(|path| path.display().to_string()),
        });
        println!(
            "{}",
            serde_json::to_string(&payload).expect("alert json serializes")
        );
        let _ = std::io::stdout().flush();
    }
}

fn phase_details(
    phase: &AgentPhase,
    state: &AgentState,
    cwd: &Path,
    timestamp: DateTime<Utc>,
) -> Value {
    match phase {
        AgentPhase::ApiHang(duration) => json!({
            "stuck_for_secs": duration.as_secs(),
        }),
        AgentPhase::ToolLoop { file, count, since } => json!({
            "file": display_path(cwd, file),
            "count": count,
            "since": format_timestamp(*since),
        }),
        AgentPhase::TestLoop {
            command,
            failure_count,
        } => json!({
            "command": command,
            "failure_count": failure_count,
        }),
        AgentPhase::TaskBlocked {
            task_id,
            blocked_by,
        } => json!({
            "task_id": task_id,
            "blocked_by": blocked_by,
        }),
        AgentPhase::ScopeLeaking {
            violating_file,
            guard_path,
        } => json!({
            "file": display_path(cwd, violating_file),
            "guard": display_path(cwd, guard_path),
        }),
        AgentPhase::ContextReset => json!({
            "compact_count": state.compact_count,
            "last_compact_at": state.last_compact_at.map(format_timestamp),
        }),
        AgentPhase::Idle(duration) => json!({
            "idle_for_secs": duration.as_secs(),
        }),
        AgentPhase::Dead => json!({
            "process_alive": state.process_alive,
            "silence_secs": compute_silence(state, timestamp).as_secs(),
            "jsonl_stale_secs": state
                .last_jsonl_record_at
                .map(|last| timestamp.signed_duration_since(last).to_std().unwrap_or_default().as_secs()),
            "token_stale_secs": state
                .last_token_activity_at
                .map(|last| timestamp.signed_duration_since(last).to_std().unwrap_or_default().as_secs()),
            "cpu_percent": state.latest_cpu_percent,
            "rss_bytes": state.latest_rss_bytes,
            "virtual_bytes": state.latest_virtual_bytes,
        }),
        _ => Value::Null,
    }
}

struct WatchActionExecutor;

impl AlertActionExecutor for WatchActionExecutor {
    fn write_stderr(&mut self, message: &str) -> Result<()> {
        eprintln!("{message}");
        std::io::stderr().flush()?;
        Ok(())
    }

    fn ring_bell(&mut self) -> Result<()> {
        eprint!("\x07");
        std::io::stderr().flush()?;
        Ok(())
    }

    fn interrupt_pid(&mut self, pid: u32) -> Result<()> {
        interrupt_pid(pid)
    }

    fn force_kill_pid(&mut self, pid: u32) -> Result<()> {
        force_kill_pid(pid)
    }

    fn save_checkpoint(&mut self, state: &AgentState, cwd: &Path) -> Result<PathBuf> {
        save_checkpoint(state, cwd)
    }
}

fn map_stuck_action(action: StuckAction) -> DaemonStuckAction {
    match action {
        StuckAction::Warn => DaemonStuckAction::Warn,
        StuckAction::Bell => DaemonStuckAction::Bell,
        StuckAction::Kill => DaemonStuckAction::Kill,
        StuckAction::CheckpointKill => DaemonStuckAction::CheckpointKill,
    }
}

fn map_scope_leak_action(action: ScopeLeakAction) -> DaemonScopeLeakAction {
    match action {
        ScopeLeakAction::Warn => DaemonScopeLeakAction::Warn,
        ScopeLeakAction::Bell => DaemonScopeLeakAction::Bell,
        ScopeLeakAction::Kill => DaemonScopeLeakAction::Kill,
    }
}

fn phase_kind(phase: &AgentPhase) -> &'static str {
    match phase {
        AgentPhase::Initializing => "initializing",
        AgentPhase::Working => "working",
        AgentPhase::Thinking => "thinking",
        AgentPhase::ApiHang(_) => "api_hang",
        AgentPhase::ToolLoop { .. } => "tool_loop",
        AgentPhase::TestLoop { .. } => "test_loop",
        AgentPhase::TaskBlocked { .. } => "task_blocked",
        AgentPhase::ContextReset => "context_reset",
        AgentPhase::ScopeLeaking { .. } => "scope_leaking",
        AgentPhase::Idle(_) => "idle",
        AgentPhase::Dead => "dead",
    }
}

fn phase_kind_changed(previous_phase: Option<&AgentPhase>, phase: &AgentPhase) -> bool {
    previous_phase
        .map(|previous| phase_kind(previous) != phase_kind(phase))
        .unwrap_or(false)
}

fn format_timestamp(timestamp: DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn colorize_phase(phase: &AgentPhase, label: &str) -> String {
    let code = match phase {
        AgentPhase::Working => 32,
        AgentPhase::Thinking => 33,
        AgentPhase::ApiHang(_)
        | AgentPhase::ToolLoop { .. }
        | AgentPhase::TestLoop { .. }
        | AgentPhase::ScopeLeaking { .. }
        | AgentPhase::Dead => 31,
        AgentPhase::ContextReset | AgentPhase::Idle(_) => 35,
        _ => 36,
    };
    format!("\x1b[{code}m{label}\x1b[0m")
}

#[cfg(test)]
mod tests {
    #![allow(clippy::field_reassign_with_default)]
    use super::{
        map_stuck_action, resolve_watch_target, status_payload, AlertNotification, ScopeLeakAction,
        StuckAction, TimeoutSetting, WatchArgs,
    };
    use crate::cmd::common::home_env_test_lock;
    use chrono::{TimeZone, Utc};
    use saw_core::{classify, AgentPhase, ClassifierConfig};
    use saw_daemon::StuckAction as DaemonStuckAction;
    use serde_json::Value;
    use std::ffi::OsString;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn classifies_scope_leak_with_guard() {
        let now = chrono::Utc::now();
        let mut state = saw_core::AgentState::default();
        state.last_event_at = Some(now);
        state.guard_paths = vec![PathBuf::from("/repo/src/auth")];
        state.latest_scope_violation = Some(PathBuf::from("/repo/src/billing/mod.rs"));

        let phase = classify(&state, now, ClassifierConfig::default());
        assert!(matches!(
            phase,
            AgentPhase::ScopeLeaking {
                ref violating_file,
                ref guard_path,
            } if violating_file == &PathBuf::from("/repo/src/billing/mod.rs")
                && guard_path == &PathBuf::from("/repo/src/auth")
        ));
    }

    #[test]
    fn classifies_tool_loop_and_test_loop() {
        let now = chrono::Utc::now();
        let mut tool_loop = saw_core::AgentState::default();
        tool_loop.last_event_at = Some(now);
        tool_loop.recent_tool_calls = std::collections::VecDeque::from(vec![
            saw_core::ToolCall {
                timestamp: now - chrono::Duration::minutes(2),
                tool_name: "Write".into(),
                file_path: Some(PathBuf::from("src/lib.rs")),
                command: None,
                line_change: None,
                is_error: false,
                is_write: true,
                is_sidechain: false,
            },
            saw_core::ToolCall {
                timestamp: now - chrono::Duration::minutes(1),
                tool_name: "Edit".into(),
                file_path: Some(PathBuf::from("src/lib.rs")),
                command: None,
                line_change: None,
                is_error: false,
                is_write: true,
                is_sidechain: false,
            },
            saw_core::ToolCall {
                timestamp: now,
                tool_name: "Write".into(),
                file_path: Some(PathBuf::from("src/lib.rs")),
                command: None,
                line_change: None,
                is_error: false,
                is_write: true,
                is_sidechain: false,
            },
        ]);
        tool_loop.apply(&saw_core::AgentEvent::FileModified(
            saw_core::FileModification {
                timestamp: now - chrono::Duration::minutes(2),
                path: PathBuf::from("src/lib.rs"),
                kind: saw_core::FileChangeKind::Modified,
                line_change: None,
            },
        ));
        tool_loop.apply(&saw_core::AgentEvent::FileModified(
            saw_core::FileModification {
                timestamp: now - chrono::Duration::minutes(1),
                path: PathBuf::from("src/lib.rs"),
                kind: saw_core::FileChangeKind::Modified,
                line_change: None,
            },
        ));
        tool_loop.apply(&saw_core::AgentEvent::FileModified(
            saw_core::FileModification {
                timestamp: now,
                path: PathBuf::from("src/lib.rs"),
                kind: saw_core::FileChangeKind::Modified,
                line_change: None,
            },
        ));

        let phase = classify(&tool_loop, now, ClassifierConfig::default());
        assert!(matches!(phase, AgentPhase::ToolLoop { count: 3, .. }));

        let mut test_loop = saw_core::AgentState::default();
        test_loop.last_event_at = Some(now);
        test_loop.recent_tool_calls = std::collections::VecDeque::from(vec![
            saw_core::ToolCall {
                timestamp: now - chrono::Duration::minutes(2),
                tool_name: "Bash".into(),
                file_path: None,
                command: Some("cargo test".into()),
                line_change: None,
                is_error: true,
                is_write: false,
                is_sidechain: false,
            },
            saw_core::ToolCall {
                timestamp: now - chrono::Duration::minutes(1),
                tool_name: "Bash".into(),
                file_path: None,
                command: Some("cargo test -p saw-core".into()),
                line_change: None,
                is_error: true,
                is_write: false,
                is_sidechain: false,
            },
            saw_core::ToolCall {
                timestamp: now,
                tool_name: "Bash".into(),
                file_path: None,
                command: Some("pytest tests/unit".into()),
                line_change: None,
                is_error: true,
                is_write: false,
                is_sidechain: false,
            },
        ]);
        test_loop.consecutive_test_failures = 3;
        test_loop.last_test_command = Some("pytest tests/unit".into());

        let phase = classify(&test_loop, now, ClassifierConfig::default());
        assert!(matches!(
            phase,
            AgentPhase::TestLoop {
                failure_count: 3,
                command,
            } if command == "pytest tests/unit"
        ));
    }

    #[test]
    fn robot_payload_contains_pid_timestamp_and_suggestion() {
        let cwd = unique_temp_dir("payload");
        let target = super::WatchTarget {
            cwd: cwd.clone(),
            pid: Some(4242),
            jsonl_path: cwd.join("session.jsonl"),
            session_id: Some("ses-1".into()),
        };
        let mut state = saw_core::AgentState::default();
        state.session_id = Some("ses-1".into());
        state.last_file_path = Some(cwd.join("src/lib.rs"));
        state.latest_cpu_percent = 0.0;
        state.last_event_at = Some(Utc::now() - chrono::Duration::seconds(121));

        let payload = status_payload(
            Some(&AgentPhase::Thinking),
            &AgentPhase::ApiHang(Duration::from_secs(121)),
            &state,
            &target,
            fixed_timestamp(),
            Some(&AlertNotification {
                timestamp: fixed_timestamp(),
                previous_phase: Some("THINKING"),
                phase: "API_HANG",
                message: "after 121s - agent appears stuck waiting on the API".into(),
                suggestion: Some("send a follow-up or use --on-stuck kill if it stays blocked"),
                action: "warn",
                checkpoint_dir: None,
            }),
        );

        assert_eq!(payload["event"], Value::String("phase_change".into()));
        assert_eq!(payload["phase"], Value::String("API_HANG".into()));
        assert_eq!(payload["previous_phase"], Value::String("THINKING".into()));
        assert_eq!(payload["pid"], Value::Number(4242.into()));
        assert_eq!(payload["file"], Value::String("src/lib.rs".into()));
        assert!(payload["timestamp"].as_str().unwrap().ends_with('Z'));
        assert_eq!(
            payload["suggestion"],
            Value::String("send a follow-up or use --on-stuck kill if it stays blocked".into())
        );
    }

    #[test]
    fn task_blocked_payload_reports_dependency_details() {
        let cwd = unique_temp_dir("task-blocked-payload");
        let target = super::WatchTarget {
            cwd: cwd.clone(),
            pid: Some(4242),
            jsonl_path: cwd.join("session.jsonl"),
            session_id: Some("ses-1".into()),
        };
        let timestamp = fixed_timestamp();
        let mut state = saw_core::AgentState::default();
        state.session_id = Some("ses-1".into());
        state.last_event_at = Some(timestamp - chrono::Duration::seconds(5));

        let payload = status_payload(
            Some(&AgentPhase::Working),
            &AgentPhase::TaskBlocked {
                task_id: "3".into(),
                blocked_by: vec!["2".into(), "4".into()],
            },
            &state,
            &target,
            timestamp,
            Some(&AlertNotification {
                timestamp,
                previous_phase: Some("WORKING"),
                phase: "TASK_BLOCKED",
                message: "task_id=3 blocked_by=2,4 - task dependencies are not completed".into(),
                suggestion: Some(
                    "complete or reassign the blocking task dependencies before continuing",
                ),
                action: "warn",
                checkpoint_dir: None,
            }),
        );

        assert_eq!(payload["phase"], Value::String("TASK_BLOCKED".into()));
        assert_eq!(payload["details"]["task_id"], Value::String("3".into()));
        assert_eq!(
            payload["details"]["blocked_by"],
            Value::Array(vec![Value::String("2".into()), Value::String("4".into())])
        );
    }

    #[test]
    fn dead_payload_reports_stale_signals() {
        let cwd = unique_temp_dir("dead-payload");
        let target = super::WatchTarget {
            cwd: cwd.clone(),
            pid: Some(4242),
            jsonl_path: cwd.join("session.jsonl"),
            session_id: Some("ses-1".into()),
        };
        let timestamp = fixed_timestamp();
        let mut state = saw_core::AgentState::default();
        state.session_id = Some("ses-1".into());
        state.last_jsonl_record_at = Some(timestamp - chrono::Duration::seconds(301));
        state.last_token_activity_at = Some(timestamp - chrono::Duration::seconds(301));
        state.last_event_at = Some(timestamp - chrono::Duration::seconds(301));
        state.latest_cpu_percent = 0.4;
        state.latest_rss_bytes = 1024;
        state.latest_virtual_bytes = 2048;

        let payload = status_payload(
            None,
            &AgentPhase::Dead,
            &state,
            &target,
            timestamp,
            Some(&AlertNotification {
                timestamp,
                previous_phase: None,
                phase: "DEAD",
                message: "process exited - Claude is no longer alive".into(),
                suggestion: Some("restart the Claude process to resume monitoring"),
                action: "kill",
                checkpoint_dir: None,
            }),
        );

        assert_eq!(payload["phase"], Value::String("DEAD".into()));
        assert_eq!(
            payload["suggestion"],
            Value::String("restart the Claude process to resume monitoring".into())
        );
        assert_eq!(payload["details"]["process_alive"], Value::Bool(true));
        assert!(payload["details"]["cpu_percent"]
            .as_f64()
            .is_some_and(|value| (value - 0.4).abs() < 0.001));
        assert_eq!(payload["details"]["jsonl_stale_secs"], Value::from(301));
        assert_eq!(payload["details"]["token_stale_secs"], Value::from(301));
    }

    #[test]
    fn checkpoint_flag_preserves_stuck_action_mapping() {
        assert_eq!(map_stuck_action(StuckAction::Warn), DaemonStuckAction::Warn);
        assert_eq!(map_stuck_action(StuckAction::Bell), DaemonStuckAction::Bell);
        assert_eq!(map_stuck_action(StuckAction::Kill), DaemonStuckAction::Kill);
        assert_eq!(
            map_stuck_action(StuckAction::CheckpointKill),
            DaemonStuckAction::CheckpointKill
        );
    }

    #[test]
    #[cfg_attr(windows, ignore = "windows session discovery is unstable in CI")]
    fn resolves_explicit_session_id() {
        let _lock = home_env_test_lock();
        let home = unique_temp_dir("watch-session-home");
        let project = unique_temp_dir("watch-session-project");
        let mut older = spawn_sleep_process();
        let mut newer = spawn_sleep_process();
        write_session_fixture(&home, &project, older.id(), "ses-older", 10);
        let expected_jsonl = write_session_fixture(&home, &project, newer.id(), "ses-newer", 20);
        let original_home = set_home(&home);

        let target = resolve_watch_target(&WatchArgs {
            file: None,
            pid: None,
            session: Some("ses-older".into()),
            dir: project.clone(),
            timeout: Some(TimeoutSetting::from_secs(130)),
            guard: vec![],
            on_stuck: Some(StuckAction::Warn),
            checkpoint: false,
            on_scope_leak: ScopeLeakAction::Warn,
            robot: false,
            quiet: false,
            no_color: true,
            force_poll: false,
        })
        .unwrap();

        assert_eq!(target.pid, Some(older.id()));
        assert_ne!(target.jsonl_path, expected_jsonl);
        assert!(target.jsonl_path.ends_with(Path::new("ses-older.jsonl")));

        restore_home(original_home);
        terminate(&mut older);
        terminate(&mut newer);
    }

    #[test]
    #[cfg_attr(windows, ignore = "windows session discovery is unstable in CI")]
    fn auto_detects_pid_and_jsonl_for_requested_project() {
        let _lock = home_env_test_lock();
        let home = unique_temp_dir("home");
        let project = unique_temp_dir("project");
        let other = unique_temp_dir("other");
        let mut wanted = spawn_sleep_process();
        let mut other_child = spawn_sleep_process();
        write_session_fixture(&home, &project, wanted.id(), "ses-wanted", 10);
        let expected_jsonl =
            write_session_fixture(&home, &other, other_child.id(), "ses-other", 20);
        let _ = expected_jsonl;
        let original_home = set_home(&home);

        let target = resolve_watch_target(&WatchArgs {
            file: None,
            pid: None,
            session: None,
            dir: project.clone(),
            timeout: Some(TimeoutSetting::from_secs(130)),
            guard: vec![],
            on_stuck: Some(StuckAction::Warn),
            checkpoint: false,
            on_scope_leak: ScopeLeakAction::Warn,
            robot: false,
            quiet: false,
            no_color: true,
            force_poll: false,
        })
        .unwrap();

        assert_eq!(target.pid, Some(wanted.id()));
        assert_eq!(target.cwd, project.canonicalize().unwrap());
        assert!(target.jsonl_path.ends_with(Path::new("ses-wanted.jsonl")));

        restore_home(original_home);
        terminate(&mut wanted);
        terminate(&mut other_child);
    }

    #[test]
    #[cfg_attr(windows, ignore = "windows session discovery is unstable in CI")]
    fn defaults_to_most_recent_active_session_in_same_project() {
        let _lock = home_env_test_lock();
        let home = unique_temp_dir("watch-most-recent-home");
        let project = unique_temp_dir("watch-most-recent-project");
        let mut older = spawn_sleep_process();
        let mut newer = spawn_sleep_process();
        let older_jsonl = write_session_fixture(&home, &project, older.id(), "ses-older", 10);
        let newer_jsonl = write_session_fixture(&home, &project, newer.id(), "ses-newer", 20);
        fs::write(&older_jsonl, "older").unwrap();
        fs::write(&newer_jsonl, "newer\nwith more activity").unwrap();
        let original_home = set_home(&home);

        let target = resolve_watch_target(&WatchArgs {
            file: None,
            pid: None,
            session: None,
            dir: project.clone(),
            timeout: Some(TimeoutSetting::from_secs(130)),
            guard: vec![],
            on_stuck: Some(StuckAction::Warn),
            checkpoint: false,
            on_scope_leak: ScopeLeakAction::Warn,
            robot: false,
            quiet: false,
            no_color: true,
            force_poll: false,
        })
        .unwrap();

        assert_eq!(target.pid, Some(newer.id()));
        assert_eq!(target.cwd, project.canonicalize().unwrap());
        assert_eq!(target.jsonl_path, newer_jsonl);

        restore_home(original_home);
        terminate(&mut older);
        terminate(&mut newer);
    }

    fn fixed_timestamp() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 24, 12, 11, 22)
            .single()
            .unwrap()
    }

    fn write_session_fixture(
        home: &Path,
        project: &Path,
        pid: u32,
        session_id: &str,
        started_at: u64,
    ) -> PathBuf {
        let sessions_dir = home.join(".claude/sessions");
        let projects_dir = home
            .join(".claude/projects")
            .join(crate::cmd::common::path_to_slug(project));
        fs::create_dir_all(&sessions_dir).unwrap();
        fs::create_dir_all(&projects_dir).unwrap();

        fs::write(
            sessions_dir.join(format!("{session_id}.json")),
            format!(
                "{{\"pid\":{pid},\"sessionId\":\"{session_id}\",\"cwd\":\"{}\",\"startedAt\":{started_at}}}",
                project.display(),
            ),
        )
        .unwrap();

        let jsonl_path = projects_dir.join(format!("{session_id}.jsonl"));
        fs::write(
            &jsonl_path,
            format!(
                "{{\"type\":\"session_started\",\"timestamp\":\"{}\",\"sessionId\":\"{session_id}\"}}\n",
                Utc::now().to_rfc3339(),
            ),
        )
        .unwrap();
        jsonl_path
    }

    fn spawn_sleep_process() -> Child {
        Command::new("python3")
            .arg("-c")
            .arg("import time; time.sleep(30)")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn terminate(child: &mut Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    fn set_home(home: &Path) -> Option<OsString> {
        let original = std::env::var_os("HOME");
        std::env::set_var("HOME", home);
        #[cfg(windows)]
        {
            std::env::set_var("USERPROFILE", home);
        }
        original
    }

    fn restore_home(original_home: Option<OsString>) {
        if let Some(home) = original_home {
            std::env::set_var("HOME", &home);
            #[cfg(windows)]
            {
                std::env::set_var("USERPROFILE", home);
            }
        } else {
            std::env::remove_var("HOME");
            #[cfg(windows)]
            {
                std::env::remove_var("USERPROFILE");
            }
        }
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("saw-{prefix}-{unique}"));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
