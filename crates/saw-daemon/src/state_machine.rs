use chrono::{DateTime, Utc};
use saw_core::{classify, AgentEvent, AgentPhase, AgentState, ClassifierConfig};

/// Applies daemon events to state and keeps the latest classifier result in sync.
#[derive(Debug, Clone)]
pub struct StateMachine {
    state: AgentState,
    classifier_config: ClassifierConfig,
}

impl StateMachine {
    pub fn new(classifier_config: ClassifierConfig) -> Self {
        Self::with_state(AgentState::default(), classifier_config)
    }

    pub fn with_state(state: AgentState, classifier_config: ClassifierConfig) -> Self {
        Self {
            state,
            classifier_config,
        }
    }

    pub fn state(&self) -> &AgentState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut AgentState {
        &mut self.state
    }

    pub fn into_state(self) -> AgentState {
        self.state
    }

    pub fn classifier_config(&self) -> ClassifierConfig {
        self.classifier_config
    }

    pub fn set_classifier_config(&mut self, classifier_config: ClassifierConfig) {
        self.classifier_config = classifier_config;
    }

    pub fn classify_at(&self, now: DateTime<Utc>) -> AgentPhase {
        classify(&self.state, now, self.classifier_config)
    }

    pub fn reclassify_at(&mut self, now: DateTime<Utc>) -> Option<AgentPhase> {
        let previous_phase = self.state.phase.clone();
        let phase = self.classify_at(now);
        let changed = phase != previous_phase;
        self.state.phase = phase.clone();
        changed.then_some(phase)
    }

    /// Applies a single event and returns the new phase when the classifier output changes.
    ///
    /// Classification runs at the event timestamp so replaying persisted event streams is
    /// deterministic and produces the same phase transitions as live processing.
    pub fn apply(&mut self, event: AgentEvent) -> Option<AgentPhase> {
        let previous_phase = self.state.phase.clone();
        let now = event.timestamp();

        self.state.apply(&event);

        let phase = self.classify_at(now);
        let changed = phase != previous_phase;
        self.state.phase = phase.clone();

        changed.then_some(phase)
    }
}

impl Default for StateMachine {
    fn default() -> Self {
        Self::new(ClassifierConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::StateMachine;
    use chrono::{Duration as ChronoDuration, TimeZone, Utc};
    use saw_core::{
        AgentEvent, AgentPhase, AgentState, ClassifierConfig, FileChangeKind, FileModification,
        ProcessMetrics, TokenActivity, ToolCall,
    };
    use std::path::PathBuf;

    fn ts(seconds_after_start: i64) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 24, 12, 0, 0)
            .single()
            .expect("valid timestamp")
            + ChronoDuration::seconds(seconds_after_start)
    }

    fn test_call(timestamp: chrono::DateTime<Utc>, command: &str) -> AgentEvent {
        AgentEvent::ToolCall(ToolCall {
            timestamp,
            tool_name: "Bash".into(),
            file_path: None,
            command: Some(command.into()),
            line_change: None,
            is_error: false,
            is_write: false,
            is_sidechain: false,
        })
    }

    #[test]
    fn applies_session_events_and_reports_phase_transitions() {
        let mut machine = StateMachine::default();

        let started = machine.apply(AgentEvent::SessionStart {
            timestamp: ts(0),
            session_id: "ses-1".into(),
        });
        assert_eq!(started, Some(AgentPhase::Working));
        assert_eq!(machine.state().session_id.as_deref(), Some("ses-1"));
        assert_eq!(machine.state().phase, AgentPhase::Working);

        let token_activity = machine.apply(AgentEvent::TokenActivity(TokenActivity {
            timestamp: ts(1),
            input_tokens: 13,
            output_tokens: 21,
            stop_reason: Some("end_turn".into()),
            is_sidechain: false,
        }));
        assert_eq!(token_activity, None);
        assert_eq!(machine.state().total_input_tokens, 13);
        assert_eq!(machine.state().total_output_tokens, 21);
        assert_eq!(machine.state().last_token_activity_at, Some(ts(1)));

        let compact = machine.apply(AgentEvent::CompactBoundary {
            timestamp: ts(2),
            is_sidechain: false,
        });
        assert_eq!(compact, Some(AgentPhase::ContextReset));
        assert!(machine.state().last_event_was_compact);
        assert_eq!(machine.state().phase, AgentPhase::ContextReset);

        let still_reset = machine.apply(AgentEvent::UserMessage {
            timestamp: ts(3),
            is_sidechain: false,
        });
        assert_eq!(still_reset, None);
        assert!(machine.state().last_event_was_compact);
        assert_eq!(machine.state().phase, AgentPhase::ContextReset);

        let resumed = machine.apply(test_call(ts(4), "cargo test -p saw-core"));
        assert_eq!(resumed, Some(AgentPhase::Working));
        assert!(!machine.state().last_event_was_compact);
        assert_eq!(machine.state().phase, AgentPhase::Working);
    }

    #[test]
    fn recent_tool_calls_buffer_is_capped_at_fifty() {
        let mut machine = StateMachine::default();
        machine.apply(AgentEvent::SessionStart {
            timestamp: ts(0),
            session_id: "ses-1".into(),
        });

        for index in 0..55 {
            machine.apply(AgentEvent::ToolCall(ToolCall {
                timestamp: ts(index + 1),
                tool_name: "Edit".into(),
                file_path: Some(PathBuf::from(format!("src/file-{index}.rs"))),
                command: None,
                line_change: None,
                is_error: false,
                is_write: true,
                is_sidechain: false,
            }));
        }

        assert_eq!(machine.state().recent_tool_calls.len(), 50);
        assert_eq!(
            machine
                .state()
                .recent_tool_calls
                .front()
                .and_then(|call| call.file_path.as_ref()),
            Some(&PathBuf::from("src/file-5.rs"))
        );
        assert_eq!(machine.state().last_tool_call_at, Some(ts(55)));
        assert_eq!(machine.state().last_event_at, Some(ts(55)));
    }

    #[test]
    fn tool_results_update_errors_and_trigger_test_loop() {
        let mut machine = StateMachine::default();
        machine.apply(AgentEvent::SessionStart {
            timestamp: ts(0),
            session_id: "ses-1".into(),
        });

        for index in 0..2 {
            machine.apply(test_call(ts(index * 2 + 1), "cargo test -p saw-core"));
            let phase = machine.apply(AgentEvent::ToolResult {
                timestamp: ts(index * 2 + 2),
                tool_name: Some("Bash".into()),
                is_error: true,
                output: Some("FAILED tests::state_machine".into()),
                stderr: Some("test failure".into()),
                interrupted: false,
                persisted_output_path: None,
                is_sidechain: false,
            });
            assert_eq!(phase, None);
        }

        machine.apply(test_call(ts(5), "cargo test -p saw-core"));
        let phase = machine.apply(AgentEvent::ToolResult {
            timestamp: ts(6),
            tool_name: Some("Bash".into()),
            is_error: true,
            output: Some("FAILED tests::state_machine".into()),
            stderr: Some("test failure".into()),
            interrupted: false,
            persisted_output_path: None,
            is_sidechain: false,
        });

        assert!(matches!(
            phase,
            Some(AgentPhase::TestLoop {
                failure_count: 3,
                ref command,
            }) if command == "cargo test -p saw-core"
        ));
        assert_eq!(machine.state().consecutive_test_failures, 3);
        assert_eq!(
            machine.state().last_test_command.as_deref(),
            Some("cargo test -p saw-core")
        );
        assert!(!machine.state().awaiting_tool_result);
        assert!(
            machine
                .state()
                .recent_tool_calls
                .back()
                .expect("recent test call")
                .is_error
        );
    }

    #[test]
    fn file_modifications_are_bounded_and_scope_leaks_change_phase() {
        let state = AgentState {
            guard_paths: vec![PathBuf::from("/repo/src/auth")],
            ..Default::default()
        };
        let mut machine = StateMachine::with_state(state, ClassifierConfig::default());

        for index in 0..21 {
            machine.apply(AgentEvent::FileModified(FileModification {
                timestamp: ts(index),
                path: PathBuf::from(format!("/repo/src/auth/file-{index}.rs")),
                kind: FileChangeKind::Modified,
                line_change: None,
            }));
        }

        let phase = machine.apply(AgentEvent::FileModified(FileModification {
            timestamp: ts(21),
            path: PathBuf::from("/repo/src/billing/mod.rs"),
            kind: FileChangeKind::Created,
            line_change: None,
        }));

        assert_eq!(machine.state().recently_modified_files.len(), 20);
        assert_eq!(
            machine
                .state()
                .recently_modified_files
                .front()
                .map(|file| file.path.clone()),
            Some(PathBuf::from("/repo/src/auth/file-2.rs"))
        );
        assert_eq!(machine.state().last_file_event_at, Some(ts(21)));
        assert_eq!(machine.state().scope_violation_count, 1);
        assert_eq!(
            machine.state().latest_scope_violation,
            Some(PathBuf::from("/repo/src/billing/mod.rs"))
        );
        assert!(matches!(
            phase,
            Some(AgentPhase::ScopeLeaking {
                ref violating_file,
                ref guard_path,
            }) if violating_file == &PathBuf::from("/repo/src/billing/mod.rs")
                && guard_path == &PathBuf::from("/repo/src/auth")
        ));
    }

    #[test]
    fn process_metrics_update_resources_and_drive_classification() {
        let mut machine = StateMachine::default();
        machine.apply(AgentEvent::SessionStart {
            timestamp: ts(0),
            session_id: "ses-1".into(),
        });
        machine.apply(test_call(ts(1), "cargo test -p saw-core"));

        let api_hang = machine.apply(AgentEvent::ProcessMetrics(ProcessMetrics {
            timestamp: ts(123),
            process_alive: true,
            cpu_percent: 0.5,
            rss_bytes: 1024,
            virtual_bytes: 2048,
            io_read_bytes: 4096,
            io_write_bytes: 8192,
            io_read_rate: 0.0,
            io_write_rate: 0.0,
        }));

        assert!(
            matches!(api_hang, Some(AgentPhase::ApiHang(duration)) if duration.as_secs() == 122)
        );
        assert_eq!(machine.state().last_process_metrics_at, Some(ts(123)));
        assert_eq!(machine.state().latest_cpu_percent, 0.5);
        assert_eq!(machine.state().latest_rss_bytes, 1024);
        assert_eq!(machine.state().latest_virtual_bytes, 2048);
        assert!(machine.state().process_alive);
        assert_eq!(machine.state().last_event_at, Some(ts(1)));

        let dead = machine.apply(AgentEvent::ProcessMetrics(ProcessMetrics {
            timestamp: ts(124),
            process_alive: false,
            cpu_percent: 0.0,
            rss_bytes: 0,
            virtual_bytes: 0,
            io_read_bytes: 0,
            io_write_bytes: 0,
            io_read_rate: 0.0,
            io_write_rate: 0.0,
        }));

        assert_eq!(dead, Some(AgentPhase::Dead));
        assert!(!machine.state().process_alive);
        assert_eq!(machine.state().phase, AgentPhase::Dead);
    }
}
