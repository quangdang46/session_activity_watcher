use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use saw_core::{AgentPhase, AgentState};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DEFAULT_ALERT_RATE_LIMIT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StuckAction {
    Warn,
    Bell,
    Kill,
    CheckpointKill,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeLeakAction {
    Warn,
    Bell,
    Kill,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlerterConfig {
    pub stuck_action: StuckAction,
    pub scope_leak_action: ScopeLeakAction,
    pub checkpoint_before_action: bool,
    pub quiet: bool,
    pub color: bool,
    pub rate_limit: Duration,
}

impl Default for AlerterConfig {
    fn default() -> Self {
        Self {
            stuck_action: StuckAction::Warn,
            scope_leak_action: ScopeLeakAction::Warn,
            checkpoint_before_action: false,
            quiet: false,
            color: true,
            rate_limit: DEFAULT_ALERT_RATE_LIMIT,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Alerter {
    config: AlerterConfig,
    last_action_at: HashMap<&'static str, DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertDecisionState {
    Executed,
    Skipped,
    Gated,
}

#[derive(Debug)]
pub struct AlertContext<'a> {
    pub timestamp: DateTime<Utc>,
    pub cwd: &'a Path,
    pub state: &'a AgentState,
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertNotification {
    pub timestamp: DateTime<Utc>,
    pub previous_phase: Option<&'static str>,
    pub phase: &'static str,
    pub message: String,
    pub suggestion: Option<&'static str>,
    pub action: &'static str,
    pub checkpoint_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertPolicyDecision {
    pub state: AlertDecisionState,
    pub action: &'static str,
    pub reason: &'static str,
    pub explanation: String,
    pub notification: Option<AlertNotification>,
}

enum ActionExecution {
    Executed {
        checkpoint_dir: Option<PathBuf>,
    },
    Gated {
        reason: &'static str,
        explanation: String,
    },
}

pub trait AlertActionExecutor {
    fn write_stderr(&mut self, _message: &str) -> Result<()> {
        Ok(())
    }
    fn ring_bell(&mut self) -> Result<()>;
    fn interrupt_pid(&mut self, pid: u32) -> Result<()>;
    fn force_kill_pid(&mut self, pid: u32) -> Result<()>;
    fn save_checkpoint(&mut self, state: &AgentState, cwd: &Path) -> Result<PathBuf>;
}

impl Alerter {
    pub fn new(config: AlerterConfig) -> Self {
        Self {
            config,
            last_action_at: HashMap::new(),
        }
    }

    pub fn should_emit_status(&self, alert: Option<&AlertNotification>) -> bool {
        !self.config.quiet || alert.is_some()
    }

    pub fn evaluate_phase_change<E: AlertActionExecutor>(
        &mut self,
        from: Option<&AgentPhase>,
        to: &AgentPhase,
        context: AlertContext<'_>,
        executor: &mut E,
    ) -> Result<AlertPolicyDecision> {
        let action = configured_action(&self.config, to);

        if !is_alert_phase(to) {
            return Ok(AlertPolicyDecision {
                state: AlertDecisionState::Skipped,
                action: "none",
                reason: "not_alert_phase",
                explanation: format!(
                    "phase {} does not trigger an automated watcher action",
                    phase_label(to)
                ),
                notification: None,
            });
        }

        if !is_new_alert(from, to) {
            return Ok(AlertPolicyDecision {
                state: AlertDecisionState::Skipped,
                action,
                reason: "no_new_alert",
                explanation: format!(
                    "phase {} did not transition into a new alert condition",
                    phase_label(to)
                ),
                notification: None,
            });
        }

        if self.is_rate_limited(action, context.timestamp) {
            return Ok(AlertPolicyDecision {
                state: AlertDecisionState::Gated,
                action,
                reason: "rate_limited",
                explanation: format!(
                    "action {action} suppressed by {}s rate limit",
                    self.config.rate_limit.as_secs()
                ),
                notification: None,
            });
        }

        match self.execute_action(to, &context, executor)? {
            ActionExecution::Executed { checkpoint_dir } => {
                let notification = AlertNotification {
                    timestamp: context.timestamp,
                    previous_phase: from.map(phase_label),
                    phase: phase_label(to),
                    message: alert_message(to, context.cwd),
                    suggestion: alert_suggestion(to),
                    action,
                    checkpoint_dir,
                };

                self.emit_action_message(executor, &notification)?;
                self.append_alert_log(context.cwd, &notification)?;
                self.last_action_at.insert(action, context.timestamp);

                Ok(AlertPolicyDecision {
                    state: AlertDecisionState::Executed,
                    action,
                    reason: "executed",
                    explanation: format!("action {action} executed for {}", phase_label(to)),
                    notification: Some(notification),
                })
            }
            ActionExecution::Gated {
                reason,
                explanation,
            } => Ok(AlertPolicyDecision {
                state: AlertDecisionState::Gated,
                action,
                reason,
                explanation,
                notification: None,
            }),
        }
    }

    pub fn on_phase_change<E: AlertActionExecutor>(
        &mut self,
        from: Option<&AgentPhase>,
        to: &AgentPhase,
        context: AlertContext<'_>,
        executor: &mut E,
    ) -> Result<Option<AlertNotification>> {
        Ok(self
            .evaluate_phase_change(from, to, context, executor)?
            .notification)
    }

    fn is_rate_limited(&self, action: &'static str, now: DateTime<Utc>) -> bool {
        self.last_action_at
            .get(action)
            .and_then(|last_seen| now.signed_duration_since(*last_seen).to_std().ok())
            .map(|elapsed| elapsed < self.config.rate_limit)
            .unwrap_or(false)
    }

    fn execute_action<E: AlertActionExecutor>(
        &self,
        phase: &AgentPhase,
        context: &AlertContext<'_>,
        executor: &mut E,
    ) -> Result<ActionExecution> {
        match phase {
            AgentPhase::ApiHang(_) | AgentPhase::ToolLoop { .. } | AgentPhase::TestLoop { .. } => {
                let checkpoint_dir = if self.config.checkpoint_before_action {
                    Some(executor.save_checkpoint(context.state, context.cwd)?)
                } else {
                    None
                };
                match self.config.stuck_action {
                    StuckAction::Warn | StuckAction::Bell => {
                        Ok(ActionExecution::Executed { checkpoint_dir })
                    }
                    StuckAction::Kill => {
                        if let Some(pid) = context.pid {
                            executor.interrupt_pid(pid)?;
                            Ok(ActionExecution::Executed { checkpoint_dir })
                        } else {
                            Ok(ActionExecution::Gated {
                                reason: "missing_pid",
                                explanation: "configured interrupt action skipped because the watcher has no live pid to signal".to_string(),
                            })
                        }
                    }
                    StuckAction::CheckpointKill => {
                        let checkpoint_dir = match checkpoint_dir {
                            Some(path) => path,
                            None => executor.save_checkpoint(context.state, context.cwd)?,
                        };
                        if let Some(pid) = context.pid {
                            executor.interrupt_pid(pid)?;
                            Ok(ActionExecution::Executed {
                                checkpoint_dir: Some(checkpoint_dir),
                            })
                        } else {
                            Ok(ActionExecution::Gated {
                                reason: "missing_pid",
                                explanation: format!(
                                    "checkpoint saved at {} but interrupt skipped because the watcher has no live pid to signal",
                                    checkpoint_dir.display()
                                ),
                            })
                        }
                    }
                }
            }
            AgentPhase::TaskBlocked { .. } | AgentPhase::ContextReset => {
                Ok(ActionExecution::Executed {
                    checkpoint_dir: None,
                })
            }
            AgentPhase::ScopeLeaking { .. } => match self.config.scope_leak_action {
                ScopeLeakAction::Warn | ScopeLeakAction::Bell => Ok(ActionExecution::Executed {
                    checkpoint_dir: None,
                }),
                ScopeLeakAction::Kill => {
                    if let Some(pid) = context.pid {
                        executor.interrupt_pid(pid)?;
                        Ok(ActionExecution::Executed {
                            checkpoint_dir: None,
                        })
                    } else {
                        Ok(ActionExecution::Gated {
                            reason: "missing_pid",
                            explanation: "configured scope-leak interrupt skipped because the watcher has no live pid to signal".to_string(),
                        })
                    }
                }
            },
            AgentPhase::Dead => match self.config.stuck_action {
                StuckAction::Warn | StuckAction::Bell => Ok(ActionExecution::Executed {
                    checkpoint_dir: None,
                }),
                StuckAction::Kill => {
                    if let Some(pid) = context.pid {
                        let _ = executor.force_kill_pid(pid);
                        Ok(ActionExecution::Executed {
                            checkpoint_dir: None,
                        })
                    } else {
                        Ok(ActionExecution::Gated {
                            reason: "missing_pid",
                            explanation: "force-kill skipped because the watcher has no pid for the exited process".to_string(),
                        })
                    }
                }
                StuckAction::CheckpointKill => {
                    if let Some(pid) = context.pid {
                        let _ = executor.force_kill_pid(pid);
                        Ok(ActionExecution::Executed {
                            checkpoint_dir: None,
                        })
                    } else {
                        Ok(ActionExecution::Gated {
                            reason: "missing_pid",
                            explanation: "checkpoint-kill skipped because the watcher has no pid for the exited process".to_string(),
                        })
                    }
                }
            },
            _ => Ok(ActionExecution::Executed {
                checkpoint_dir: None,
            }),
        }
    }

    fn emit_action_message<E: AlertActionExecutor>(
        &self,
        executor: &mut E,
        notification: &AlertNotification,
    ) -> Result<()> {
        executor.write_stderr(&notification.render_stderr(self.config.color))?;
        if notification.action == "bell" {
            executor.ring_bell()?;
        }
        Ok(())
    }

    fn append_alert_log(&self, cwd: &Path, notification: &AlertNotification) -> Result<()> {
        let log_path = alert_log_path(cwd);
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .with_context(|| format!("failed to open {}", log_path.display()))?;
        writeln!(file, "{}", notification.render_human())
            .with_context(|| format!("failed to append {}", log_path.display()))?;
        Ok(())
    }
}

impl AlertNotification {
    pub fn render_human(&self) -> String {
        render_notification_line(self, false)
    }

    pub fn render_stderr(&self, color: bool) -> String {
        render_notification_line(self, color)
    }
}

pub fn alert_log_path(cwd: &Path) -> PathBuf {
    cwd.join(".saw/alerts.log")
}

fn render_notification_line(notification: &AlertNotification, color: bool) -> String {
    let phase = if color {
        colorize_phase(notification.phase)
    } else {
        notification.phase.to_string()
    };
    let mut line = format!(
        "{} ALERT {} {} action={}",
        format_timestamp(notification.timestamp),
        phase,
        notification.message,
        notification.action
    );

    if let Some(previous_phase) = notification.previous_phase {
        line.push_str(&format!(" from={previous_phase}"));
    }

    if let Some(suggestion) = notification.suggestion {
        line.push_str(&format!(" suggestion=\"{suggestion}\""));
    }

    if let Some(checkpoint_dir) = notification.checkpoint_dir.as_ref() {
        line.push_str(&format!(" checkpoint={}", checkpoint_dir.display()));
    }

    line
}

fn colorize_phase(phase: &str) -> String {
    let color = match phase {
        "DEAD" => "31",
        "TASK_BLOCKED" => "33",
        _ => "33",
    };
    format!("\x1b[1;{color}m{phase}\x1b[0m")
}

fn configured_action(config: &AlerterConfig, phase: &AgentPhase) -> &'static str {
    match phase {
        AgentPhase::ApiHang(_) | AgentPhase::ToolLoop { .. } | AgentPhase::TestLoop { .. } => {
            match config.stuck_action {
                StuckAction::Warn => "warn",
                StuckAction::Bell => "bell",
                StuckAction::Kill => "kill",
                StuckAction::CheckpointKill => "checkpoint-kill",
            }
        }
        AgentPhase::TaskBlocked { .. } => "warn",
        AgentPhase::ContextReset => "warn",
        AgentPhase::ScopeLeaking { .. } => match config.scope_leak_action {
            ScopeLeakAction::Warn => "warn",
            ScopeLeakAction::Bell => "bell",
            ScopeLeakAction::Kill => "kill",
        },
        AgentPhase::Dead => match config.stuck_action {
            StuckAction::Warn => "warn",
            StuckAction::Bell => "bell",
            StuckAction::Kill => "kill",
            StuckAction::CheckpointKill => "checkpoint-kill",
        },
        _ => "warn",
    }
}

fn is_new_alert(previous_phase: Option<&AgentPhase>, phase: &AgentPhase) -> bool {
    is_alert_phase(phase)
        && previous_phase
            .map(|previous| phase_key(previous) != phase_key(phase))
            .unwrap_or(true)
}

fn is_alert_phase(phase: &AgentPhase) -> bool {
    matches!(
        phase,
        AgentPhase::ApiHang(_)
            | AgentPhase::ToolLoop { .. }
            | AgentPhase::TestLoop { .. }
            | AgentPhase::TaskBlocked { .. }
            | AgentPhase::ContextReset
            | AgentPhase::ScopeLeaking { .. }
            | AgentPhase::Dead
    )
}

fn phase_key(phase: &AgentPhase) -> &'static str {
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

fn phase_label(phase: &AgentPhase) -> &'static str {
    match phase {
        AgentPhase::Initializing => "INITIALIZING",
        AgentPhase::Working => "WORKING",
        AgentPhase::Thinking => "THINKING",
        AgentPhase::ApiHang(_) => "API_HANG",
        AgentPhase::ToolLoop { .. } => "TOOL_LOOP",
        AgentPhase::TestLoop { .. } => "TEST_LOOP",
        AgentPhase::TaskBlocked { .. } => "TASK_BLOCKED",
        AgentPhase::ContextReset => "CONTEXT_RESET",
        AgentPhase::ScopeLeaking { .. } => "SCOPE_LEAKING",
        AgentPhase::Idle(_) => "IDLE",
        AgentPhase::Dead => "DEAD",
    }
}

fn alert_message(phase: &AgentPhase, cwd: &Path) -> String {
    match phase {
        AgentPhase::ApiHang(duration) => {
            format!(
                "after {}s - agent appears stuck waiting on the API",
                duration.as_secs()
            )
        }
        AgentPhase::ToolLoop { file, count, .. } => format!(
            "file={} rewrites={} - same file keeps being rewritten",
            display_path(cwd, file),
            count
        ),
        AgentPhase::TestLoop {
            command,
            failure_count,
        } => format!(
            "command=\"{}\" failures={} - repeated failing test command",
            command, failure_count
        ),
        AgentPhase::TaskBlocked {
            task_id,
            blocked_by,
        } => format!(
            "task_id={} blocked_by={} - task dependencies are not completed",
            task_id,
            blocked_by.join(",")
        ),
        AgentPhase::ContextReset => {
            "Context was compacted - earlier constraints may be lost".to_string()
        }
        AgentPhase::ScopeLeaking {
            violating_file,
            guard_path,
        } => format!(
            "file={} guard={} - edited outside the configured guard",
            display_path(cwd, violating_file),
            display_path(cwd, guard_path)
        ),
        AgentPhase::Dead => "process exited - Claude is no longer alive".to_string(),
        _ => "unexpected alert transition".to_string(),
    }
}

fn alert_suggestion(phase: &AgentPhase) -> Option<&'static str> {
    match phase {
        AgentPhase::ApiHang(_) => {
            Some("send a follow-up or use --on-stuck kill/--checkpoint if it stays blocked")
        }
        AgentPhase::ToolLoop { .. } => {
            Some("inspect the repeated write target before interrupting")
        }
        AgentPhase::TestLoop { .. } => Some("fix the failing test command before rerunning it"),
        AgentPhase::TaskBlocked { .. } => {
            Some("complete or reassign the blocking task dependencies before continuing")
        }
        AgentPhase::ContextReset => {
            Some("restate any critical constraints before the session resumes work")
        }
        AgentPhase::ScopeLeaking { .. } => {
            Some("tighten --guard or stop the session to limit drift")
        }
        AgentPhase::Dead => Some("restart the Claude process to resume monitoring"),
        _ => None,
    }
}

fn display_path(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd)
        .map(|relative| relative.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

fn format_timestamp(timestamp: DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::{
        alert_log_path, AlertActionExecutor, AlertContext, AlertDecisionState, AlertNotification,
        Alerter, AlerterConfig, ScopeLeakAction, StuckAction,
    };
    use chrono::{Duration as ChronoDuration, TimeZone, Utc};
    use saw_core::{AgentPhase, AgentState};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[derive(Debug, Default)]
    struct MockExecutor {
        stderr_messages: Vec<String>,
        bells: usize,
        interrupts: Vec<u32>,
        force_kills: Vec<u32>,
        checkpoints: Vec<PathBuf>,
    }

    impl AlertActionExecutor for MockExecutor {
        fn write_stderr(&mut self, message: &str) -> anyhow::Result<()> {
            self.stderr_messages.push(message.to_string());
            Ok(())
        }

        fn ring_bell(&mut self) -> anyhow::Result<()> {
            self.bells += 1;
            Ok(())
        }

        fn interrupt_pid(&mut self, pid: u32) -> anyhow::Result<()> {
            self.interrupts.push(pid);
            Ok(())
        }

        fn force_kill_pid(&mut self, pid: u32) -> anyhow::Result<()> {
            self.force_kills.push(pid);
            Ok(())
        }

        fn save_checkpoint(&mut self, _state: &AgentState, cwd: &Path) -> anyhow::Result<PathBuf> {
            let path = cwd.join(".saw/checkpoints/mock-checkpoint");
            self.checkpoints.push(path.clone());
            Ok(path)
        }
    }

    #[test]
    fn formats_human_readable_api_hang_message_and_logs_it() {
        let cwd = unique_temp_dir("alert-log");
        let state = AgentState::default();
        let mut alerter = Alerter::new(AlerterConfig::default());
        let mut executor = MockExecutor::default();
        let timestamp = ts(0);

        let alert = alerter
            .on_phase_change(
                Some(&AgentPhase::Thinking),
                &AgentPhase::ApiHang(Duration::from_secs(121)),
                AlertContext {
                    timestamp,
                    cwd: &cwd,
                    state: &state,
                    pid: Some(4242),
                },
                &mut executor,
            )
            .unwrap()
            .expect("expected alert");

        assert_eq!(alert.phase, "API_HANG");
        assert_eq!(alert.action, "warn");
        assert!(alert.message.contains("after 121s"));
        assert!(alert
            .suggestion
            .expect("suggestion")
            .contains("--on-stuck kill/--checkpoint"));

        assert_eq!(executor.stderr_messages.len(), 1);
        assert!(executor.stderr_messages[0].contains("API_HANG"));
        assert!(executor.stderr_messages[0].contains("--on-stuck kill/--checkpoint"));

        let log = fs::read_to_string(alert_log_path(&cwd)).unwrap();
        assert!(log.contains("ALERT API_HANG after 121s"));
        assert!(log.contains("action=warn"));
    }

    #[test]
    fn warns_for_task_blocked_without_executing_actions() {
        let cwd = unique_temp_dir("task-blocked-alert");
        let state = AgentState::default();
        let mut alerter = Alerter::new(AlerterConfig {
            stuck_action: StuckAction::CheckpointKill,
            ..AlerterConfig::default()
        });
        let mut executor = MockExecutor::default();

        let alert = alerter
            .on_phase_change(
                Some(&AgentPhase::Working),
                &AgentPhase::TaskBlocked {
                    task_id: "3".into(),
                    blocked_by: vec!["2".into(), "4".into()],
                },
                AlertContext {
                    timestamp: ts(0),
                    cwd: &cwd,
                    state: &state,
                    pid: Some(321),
                },
                &mut executor,
            )
            .unwrap()
            .expect("expected alert");

        assert_eq!(alert.action, "warn");
        assert!(alert.message.contains("task_id=3"));
        assert!(alert.message.contains("blocked_by=2,4"));
        assert!(alert
            .suggestion
            .expect("suggestion")
            .contains("blocking task dependencies"));
        assert!(executor.interrupts.is_empty());
        assert!(executor.checkpoints.is_empty());
    }

    #[test]
    fn warns_for_context_reset_without_executing_actions() {
        let cwd = unique_temp_dir("context-reset-alert");
        let state = AgentState {
            compact_count: 2,
            ..Default::default()
        };
        let mut alerter = Alerter::new(AlerterConfig {
            stuck_action: StuckAction::CheckpointKill,
            ..AlerterConfig::default()
        });
        let mut executor = MockExecutor::default();

        let alert = alerter
            .on_phase_change(
                Some(&AgentPhase::Working),
                &AgentPhase::ContextReset,
                AlertContext {
                    timestamp: ts(0),
                    cwd: &cwd,
                    state: &state,
                    pid: Some(321),
                },
                &mut executor,
            )
            .unwrap()
            .expect("expected alert");

        assert_eq!(alert.action, "warn");
        assert_eq!(
            alert.message,
            "Context was compacted - earlier constraints may be lost"
        );
        assert!(alert
            .suggestion
            .expect("suggestion")
            .contains("restate any critical constraints"));
        assert!(executor.interrupts.is_empty());
        assert!(executor.checkpoints.is_empty());
    }

    #[test]
    fn rate_limits_repeat_alerts_for_same_action() {
        let cwd = unique_temp_dir("rate-limit");
        let state = AgentState::default();
        let mut alerter = Alerter::new(AlerterConfig::default());
        let mut executor = MockExecutor::default();
        let phase = AgentPhase::ApiHang(Duration::from_secs(121));

        let first = alerter
            .on_phase_change(
                Some(&AgentPhase::Thinking),
                &phase,
                AlertContext {
                    timestamp: ts(0),
                    cwd: &cwd,
                    state: &state,
                    pid: Some(123),
                },
                &mut executor,
            )
            .unwrap();
        assert!(first.is_some());

        let suppressed = alerter
            .on_phase_change(
                Some(&AgentPhase::Working),
                &phase,
                AlertContext {
                    timestamp: ts(30),
                    cwd: &cwd,
                    state: &state,
                    pid: Some(123),
                },
                &mut executor,
            )
            .unwrap();
        assert!(suppressed.is_none());

        let second = alerter
            .on_phase_change(
                Some(&AgentPhase::Working),
                &phase,
                AlertContext {
                    timestamp: ts(61),
                    cwd: &cwd,
                    state: &state,
                    pid: Some(123),
                },
                &mut executor,
            )
            .unwrap();
        assert!(second.is_some());
    }

    #[test]
    fn policy_decision_reports_executed_action() {
        let cwd = unique_temp_dir("policy-executed");
        let state = AgentState::default();
        let mut alerter = Alerter::new(AlerterConfig::default());
        let mut executor = MockExecutor::default();

        let decision = alerter
            .evaluate_phase_change(
                None,
                &AgentPhase::ApiHang(Duration::from_secs(121)),
                AlertContext {
                    timestamp: ts(0),
                    cwd: &cwd,
                    state: &state,
                    pid: Some(4242),
                },
                &mut executor,
            )
            .unwrap();

        assert_eq!(decision.state, AlertDecisionState::Executed);
        assert_eq!(decision.reason, "executed");
        assert_eq!(decision.action, "warn");
        assert!(decision.notification.is_some());
    }

    #[test]
    fn policy_decision_reports_skipped_action() {
        let cwd = unique_temp_dir("policy-skipped");
        let state = AgentState::default();
        let mut alerter = Alerter::new(AlerterConfig::default());
        let mut executor = MockExecutor::default();
        let phase = AgentPhase::ApiHang(Duration::from_secs(121));

        let decision = alerter
            .evaluate_phase_change(
                Some(&phase),
                &phase,
                AlertContext {
                    timestamp: ts(0),
                    cwd: &cwd,
                    state: &state,
                    pid: Some(4242),
                },
                &mut executor,
            )
            .unwrap();

        assert_eq!(decision.state, AlertDecisionState::Skipped);
        assert_eq!(decision.reason, "no_new_alert");
        assert!(decision.notification.is_none());
    }

    #[test]
    fn policy_decision_reports_gated_action() {
        let cwd = unique_temp_dir("policy-gated");
        let state = AgentState::default();
        let mut alerter = Alerter::new(AlerterConfig {
            stuck_action: StuckAction::Kill,
            ..AlerterConfig::default()
        });
        let mut executor = MockExecutor::default();

        let decision = alerter
            .evaluate_phase_change(
                None,
                &AgentPhase::ApiHang(Duration::from_secs(121)),
                AlertContext {
                    timestamp: ts(0),
                    cwd: &cwd,
                    state: &state,
                    pid: None,
                },
                &mut executor,
            )
            .unwrap();

        assert_eq!(decision.state, AlertDecisionState::Gated);
        assert_eq!(decision.reason, "missing_pid");
        assert_eq!(decision.action, "kill");
        assert!(decision.notification.is_none());
    }

    #[test]
    fn rings_bell_and_emits_stderr_for_stuck_alerts() {
        let cwd = unique_temp_dir("bell-alert");
        let state = AgentState::default();
        let mut alerter = Alerter::new(AlerterConfig {
            stuck_action: StuckAction::Bell,
            color: false,
            ..AlerterConfig::default()
        });
        let mut executor = MockExecutor::default();

        let alert = alerter
            .on_phase_change(
                Some(&AgentPhase::Thinking),
                &AgentPhase::ApiHang(Duration::from_secs(121)),
                AlertContext {
                    timestamp: ts(0),
                    cwd: &cwd,
                    state: &state,
                    pid: Some(5150),
                },
                &mut executor,
            )
            .unwrap()
            .expect("expected alert");

        assert_eq!(alert.action, "bell");
        assert_eq!(executor.bells, 1);
        assert_eq!(executor.stderr_messages.len(), 1);
        assert!(executor.stderr_messages[0].contains("ALERT API_HANG"));
    }

    #[test]
    fn executes_stuck_kill_action() {
        let cwd = unique_temp_dir("stuck-kill");
        let state = AgentState::default();
        let mut alerter = Alerter::new(AlerterConfig {
            stuck_action: StuckAction::Kill,
            ..AlerterConfig::default()
        });
        let mut executor = MockExecutor::default();

        let alert = alerter
            .on_phase_change(
                Some(&AgentPhase::Thinking),
                &AgentPhase::ApiHang(Duration::from_secs(121)),
                AlertContext {
                    timestamp: ts(0),
                    cwd: &cwd,
                    state: &state,
                    pid: Some(777),
                },
                &mut executor,
            )
            .unwrap()
            .expect("expected alert");

        assert_eq!(alert.action, "kill");
        assert_eq!(executor.interrupts, vec![777]);
        assert!(executor.checkpoints.is_empty());
        assert_eq!(executor.stderr_messages.len(), 1);
        assert!(executor.stderr_messages[0].contains("API_HANG"));
    }

    #[test]
    fn executes_checkpoint_kill_for_stuck_alerts() {
        let cwd = unique_temp_dir("checkpoint-kill");
        let state = AgentState::default();
        let mut alerter = Alerter::new(AlerterConfig {
            stuck_action: StuckAction::CheckpointKill,
            ..AlerterConfig::default()
        });
        let mut executor = MockExecutor::default();

        let alert = alerter
            .on_phase_change(
                Some(&AgentPhase::Thinking),
                &AgentPhase::ToolLoop {
                    file: cwd.join("src/lib.rs"),
                    count: 4,
                    since: ts(-30),
                },
                AlertContext {
                    timestamp: ts(0),
                    cwd: &cwd,
                    state: &state,
                    pid: Some(777),
                },
                &mut executor,
            )
            .unwrap()
            .expect("expected alert");

        assert_eq!(alert.action, "checkpoint-kill");
        assert!(alert.checkpoint_dir.is_some());
        assert_eq!(executor.interrupts, vec![777]);
        assert_eq!(executor.checkpoints.len(), 1);
        assert_eq!(executor.stderr_messages.len(), 1);
        assert!(executor.stderr_messages[0].contains("checkpoint-kill"));
    }

    #[test]
    fn checkpoints_before_warn_when_enabled() {
        let cwd = unique_temp_dir("checkpoint-warn");
        let state = AgentState::default();
        let mut alerter = Alerter::new(AlerterConfig {
            checkpoint_before_action: true,
            ..AlerterConfig::default()
        });
        let mut executor = MockExecutor::default();

        let alert = alerter
            .on_phase_change(
                Some(&AgentPhase::Thinking),
                &AgentPhase::ApiHang(Duration::from_secs(121)),
                AlertContext {
                    timestamp: ts(0),
                    cwd: &cwd,
                    state: &state,
                    pid: Some(777),
                },
                &mut executor,
            )
            .unwrap()
            .expect("expected alert");

        assert_eq!(alert.action, "warn");
        assert!(alert.checkpoint_dir.is_some());
        assert!(executor.interrupts.is_empty());
        assert_eq!(executor.checkpoints.len(), 1);
    }

    #[test]
    fn checkpoints_before_kill_when_enabled() {
        let cwd = unique_temp_dir("checkpoint-before-kill");
        let state = AgentState::default();
        let mut alerter = Alerter::new(AlerterConfig {
            stuck_action: StuckAction::Kill,
            checkpoint_before_action: true,
            ..AlerterConfig::default()
        });
        let mut executor = MockExecutor::default();

        let alert = alerter
            .on_phase_change(
                Some(&AgentPhase::Thinking),
                &AgentPhase::TestLoop {
                    command: "cargo test".into(),
                    failure_count: 3,
                },
                AlertContext {
                    timestamp: ts(0),
                    cwd: &cwd,
                    state: &state,
                    pid: Some(777),
                },
                &mut executor,
            )
            .unwrap()
            .expect("expected alert");

        assert_eq!(alert.action, "kill");
        assert!(alert.checkpoint_dir.is_some());
        assert_eq!(executor.interrupts, vec![777]);
        assert_eq!(executor.checkpoints.len(), 1);
    }

    #[test]
    fn executes_scope_leak_kill_action() {
        let cwd = unique_temp_dir("scope-kill");
        let state = AgentState::default();
        let mut alerter = Alerter::new(AlerterConfig {
            scope_leak_action: ScopeLeakAction::Kill,
            ..AlerterConfig::default()
        });
        let mut executor = MockExecutor::default();

        let alert = alerter
            .on_phase_change(
                Some(&AgentPhase::Working),
                &AgentPhase::ScopeLeaking {
                    violating_file: cwd.join("src/billing/mod.rs"),
                    guard_path: cwd.join("src/auth"),
                },
                AlertContext {
                    timestamp: ts(0),
                    cwd: &cwd,
                    state: &state,
                    pid: Some(991),
                },
                &mut executor,
            )
            .unwrap()
            .expect("expected alert");

        assert_eq!(alert.action, "kill");
        assert_eq!(executor.interrupts, vec![991]);
        assert_eq!(executor.stderr_messages.len(), 1);
        assert!(executor.stderr_messages[0].contains("SCOPE_LEAKING"));
        assert!(alert.message.contains("src/billing/mod.rs"));
        assert!(alert.message.contains("src/auth"));
    }

    #[test]
    fn quiet_mode_only_emits_when_alert_exists() {
        let alerter = Alerter::new(AlerterConfig {
            quiet: true,
            ..AlerterConfig::default()
        });

        assert!(!alerter.should_emit_status(None));
        assert!(alerter.should_emit_status(Some(&AlertNotification {
            timestamp: ts(0),
            previous_phase: Some("THINKING"),
            phase: "API_HANG",
            message: "after 121s - agent appears stuck waiting on the API".into(),
            suggestion: Some("send a follow-up"),
            action: "warn",
            checkpoint_dir: None,
        })));
    }

    fn ts(seconds_after_start: i64) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 24, 12, 0, 0)
            .single()
            .expect("valid timestamp")
            + ChronoDuration::seconds(seconds_after_start)
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("saw-daemon-{prefix}-{unique}"));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
