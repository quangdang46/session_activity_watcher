use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use saw_core::{
    classify, AgentEvent, AgentPhase, AgentState, ClassifierConfig, ProcessMetrics, SessionRecord,
};
use serde::Deserialize;
use std::cmp::Reverse;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::{
    EventBus, FileWatcher, JsonlTailer, JsonlTailerOptions, ProcessMonitor, Receiver, StateMachine,
};

#[derive(Debug, Clone)]
pub struct WatcherRuntimeOptions {
    pub home_dir: PathBuf,
    pub cwd: PathBuf,
    pub file: Option<PathBuf>,
    pub pid: Option<u32>,
    pub session_id: Option<String>,
    pub guard_paths: Vec<PathBuf>,
    pub force_poll: bool,
    pub classifier_config: ClassifierConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatcherRuntimeTarget {
    pub cwd: PathBuf,
    pub pid: Option<u32>,
    pub jsonl_path: PathBuf,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct SessionFile {
    pub pid: u32,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    pub cwd: String,
    #[serde(rename = "startedAt")]
    pub started_at: u64,
    #[serde(skip)]
    pub session_file_path: Option<PathBuf>,
}

impl SessionFile {
    pub fn cwd_path(&self) -> PathBuf {
        normalize_path(Path::new(&self.cwd))
    }
}

#[derive(Debug, Clone)]
pub struct SessionSelection {
    pub pid: u32,
    pub session: SessionFile,
    pub jsonl_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct RuntimeUpdate {
    pub timestamp: DateTime<Utc>,
    pub event: Option<AgentEvent>,
    pub previous_phase: Option<AgentPhase>,
    pub phase: AgentPhase,
    pub state: AgentState,
    pub target: WatcherRuntimeTarget,
}

impl RuntimeUpdate {
    pub fn phase_changed(&self) -> bool {
        self.previous_phase
            .as_ref()
            .map(|phase| phase != &self.phase)
            .unwrap_or(false)
    }
}

pub trait RuntimeStateRefresher: Send + Sync {
    fn refresh(&self, state: &mut AgentState, cwd: &Path) -> Result<()>;
}

#[derive(Debug, Default)]
pub struct NoopRuntimeStateRefresher;

impl RuntimeStateRefresher for NoopRuntimeStateRefresher {
    fn refresh(&self, _state: &mut AgentState, _cwd: &Path) -> Result<()> {
        Ok(())
    }
}

pub struct WatcherRuntime {
    target: WatcherRuntimeTarget,
    state_machine: StateMachine,
    event_receiver: Receiver<AgentEvent>,
    update_sender: broadcast::Sender<RuntimeUpdate>,
    refresher: Arc<dyn RuntimeStateRefresher>,
    last_timestamp: DateTime<Utc>,
    tail_bridge: Option<JoinHandle<()>>,
    tailer_task: Option<JoinHandle<Result<()>>>,
    monitor_task: Option<JoinHandle<()>>,
    file_watcher: Option<FileWatcher>,
    shutdown: bool,
}

impl WatcherRuntime {
    pub fn attach(
        options: WatcherRuntimeOptions,
        refresher: Arc<dyn RuntimeStateRefresher>,
    ) -> Result<Self> {
        let target = resolve_watch_target(&options)?;
        let mut state = load_initial_state(&target, &options, refresher.as_ref())?;
        let now = Utc::now();
        state.phase = classify(&state, now, options.classifier_config);
        let state_machine = StateMachine::with_state(state, options.classifier_config);

        let bus = EventBus::new();
        let event_receiver = bus.subscribe();
        let (update_sender, _) = broadcast::channel(64);

        let (tail_tx, mut tail_rx) = mpsc::channel(256);
        let tail_bus = bus.clone();
        let tail_bridge = tokio::spawn(async move {
            while let Some(event) = tail_rx.recv().await {
                tail_bus.publish(event);
            }
        });

        let mut tailer =
            JsonlTailer::with_options(&target.jsonl_path, tailer_options(&options, target.pid))
                .with_context(|| format!("failed to watch {}", target.jsonl_path.display()))?;
        tailer.set_byte_offset(current_offset(&target.jsonl_path));
        let tailer_task = tokio::spawn(async move { tailer.run(tail_tx).await });

        let monitor_task = target.pid.map(|pid| {
            let monitor_bus = bus.clone();
            tokio::spawn(async move {
                let mut monitor = ProcessMonitor::new(pid);
                monitor.run(|event| monitor_bus.publish(event)).await;
            })
        });

        let file_watcher = Some(
            FileWatcher::new(&target.cwd, state_machine.state().guard_paths.clone(), bus)
                .with_context(|| {
                    format!(
                        "failed to watch filesystem events under {}",
                        target.cwd.display()
                    )
                })?,
        );

        Ok(Self {
            target,
            state_machine,
            event_receiver,
            update_sender,
            refresher,
            last_timestamp: now,
            tail_bridge: Some(tail_bridge),
            tailer_task: Some(tailer_task),
            monitor_task,
            file_watcher,
            shutdown: false,
        })
    }

    pub fn target(&self) -> &WatcherRuntimeTarget {
        &self.target
    }

    pub fn initial_update(&self) -> RuntimeUpdate {
        RuntimeUpdate {
            timestamp: self.last_timestamp,
            event: None,
            previous_phase: None,
            phase: self.state_machine.state().phase.clone(),
            state: self.state_machine.state().clone(),
            target: self.target.clone(),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<RuntimeUpdate> {
        self.update_sender.subscribe()
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown
    }

    pub async fn next_update(&mut self) -> Result<Option<RuntimeUpdate>> {
        if self.shutdown {
            return Ok(None);
        }

        match self.event_receiver.recv().await {
            Ok(event) => {
                let timestamp = event.timestamp();
                let previous_phase = self.state_machine.state().phase.clone();
                self.state_machine.state_mut().apply(&event);
                if !matches!(event, AgentEvent::ProcessMetrics(_)) {
                    self.refresher
                        .refresh(self.state_machine.state_mut(), &self.target.cwd)?;
                }
                let _ = self.state_machine.reclassify_at(timestamp);
                self.last_timestamp = timestamp;

                let update = RuntimeUpdate {
                    timestamp,
                    event: Some(event),
                    previous_phase: Some(previous_phase),
                    phase: self.state_machine.state().phase.clone(),
                    state: self.state_machine.state().clone(),
                    target: self.target.clone(),
                };
                let _ = self.update_sender.send(update.clone());
                Ok(Some(update))
            }
            Err(_) => Ok(None),
        }
    }

    pub async fn shutdown(&mut self) {
        if self.shutdown {
            return;
        }

        self.shutdown = true;
        if let Some(task) = self.tailer_task.as_ref() {
            task.abort();
        }
        if let Some(task) = self.monitor_task.take() {
            task.abort();
            let _ = task.await;
        }
        if let Some(task) = self.tail_bridge.as_ref() {
            task.abort();
        }
        if let Some(task) = self.tailer_task.take() {
            let _ = task.await;
        }
        if let Some(task) = self.tail_bridge.take() {
            let _ = task.await;
        }
        self.file_watcher = None;
    }
}

impl Drop for WatcherRuntime {
    fn drop(&mut self) {
        if self.shutdown {
            return;
        }

        self.shutdown = true;
        if let Some(task) = self.tailer_task.as_ref() {
            task.abort();
        }
        if let Some(task) = self.monitor_task.take() {
            task.abort();
        }
        if let Some(task) = self.tail_bridge.as_ref() {
            task.abort();
        }
        self.file_watcher = None;
    }
}

pub fn resolve_watch_target(options: &WatcherRuntimeOptions) -> Result<WatcherRuntimeTarget> {
    let requested_cwd = options
        .cwd
        .canonicalize()
        .unwrap_or_else(|_| options.cwd.clone());

    if let Some(file) = options.file.as_ref() {
        return Ok(WatcherRuntimeTarget {
            cwd: requested_cwd,
            pid: options.pid,
            jsonl_path: file.clone(),
            session_id: options.session_id.clone(),
        });
    }

    let selection = find_session_for_pid_or_cwd(
        &options.home_dir,
        &requested_cwd,
        options.pid,
        options.session_id.as_deref(),
    )?;

    Ok(WatcherRuntimeTarget {
        cwd: selection.session.cwd_path(),
        pid: Some(selection.pid),
        jsonl_path: selection.jsonl_path,
        session_id: Some(selection.session.session_id),
    })
}

fn load_initial_state(
    target: &WatcherRuntimeTarget,
    options: &WatcherRuntimeOptions,
    refresher: &dyn RuntimeStateRefresher,
) -> Result<AgentState> {
    let mut state = load_existing_state(&target.jsonl_path)?;
    state.guard_paths = normalize_guard_paths(&target.cwd, &options.guard_paths);
    state.session_jsonl_path = Some(target.jsonl_path.clone());

    if state.session_id.is_none() {
        state.session_id = target.session_id.clone();
    }

    if let Some(pid) = target.pid {
        match sample_process_metrics(pid) {
            Some(metrics) => state.apply(&AgentEvent::ProcessMetrics(metrics)),
            None => state.process_alive = false,
        }
    }

    refresher.refresh(&mut state, &target.cwd)?;
    Ok(state)
}

fn load_existing_state(path: &Path) -> Result<AgentState> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut state = AgentState::default();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        if let Some(event) = SessionRecord::parse(&line) {
            state.apply(&event);
        }
    }

    Ok(state)
}

fn find_session_for_pid_or_cwd(
    home: &Path,
    cwd: &Path,
    pid: Option<u32>,
    session_id: Option<&str>,
) -> Result<SessionSelection> {
    let sessions = list_alive_session_selections(home)?;

    if let Some(pid) = pid {
        return sessions
            .into_iter()
            .find(|selection| selection.pid == pid)
            .with_context(|| format!("no running Claude Code session found for pid {pid}"));
    }

    if let Some(session_id) = session_id {
        return sessions
            .into_iter()
            .find(|selection| selection.session.session_id == session_id)
            .with_context(|| {
                format!("no running Claude Code session found for session {session_id}")
            });
    }

    sessions_for_cwd(&sessions, Some(cwd))
        .into_iter()
        .next()
        .context(format!(
            "no running Claude Code sessions found in ~/.claude/sessions for {}",
            cwd.display()
        ))
}

pub fn list_alive_session_selections(home: &Path) -> Result<Vec<SessionSelection>> {
    let mut selections = list_alive_sessions(home)?
        .into_iter()
        .filter_map(|(pid, session)| {
            let jsonl_path = session_jsonl_path(home, &session).ok()?;
            Some(SessionSelection {
                pid,
                session,
                jsonl_path,
            })
        })
        .collect::<Vec<_>>();

    selections.sort_by_key(|selection| {
        Reverse((
            session_activity_key(selection),
            selection.session.started_at,
        ))
    });
    Ok(selections)
}

pub fn sessions_for_cwd(
    sessions: &[SessionSelection],
    cwd: Option<&Path>,
) -> Vec<SessionSelection> {
    let preferred_cwd = cwd.map(normalize_path);

    match preferred_cwd.as_deref() {
        Some(preferred) => sessions
            .iter()
            .filter(|selection| paths_match(selection.session.cwd_path().as_path(), preferred))
            .cloned()
            .collect(),
        None => sessions.to_vec(),
    }
}

fn list_alive_sessions(home: &Path) -> Result<Vec<(u32, SessionFile)>> {
    let sessions_dir = home.join(".claude/sessions");
    let entries = std::fs::read_dir(&sessions_dir)
        .with_context(|| format!("failed to read {}", sessions_dir.display()))?;
    let mut system = System::new();
    let mut sessions = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension() != Some(OsStr::new("json")) {
            continue;
        }

        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(_) => continue,
        };
        let mut session: SessionFile = match serde_json::from_str(&raw) {
            Ok(session) => session,
            Err(_) => continue,
        };
        session.session_file_path = Some(path.clone());

        if is_pid_alive(&mut system, session.pid) {
            sessions.push((session.pid, session));
        }
    }

    Ok(sessions)
}

fn is_pid_alive(system: &mut System, pid: u32) -> bool {
    let pid = Pid::from_u32(pid);
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    system.process(pid).is_some()
}

fn session_activity_key(selection: &SessionSelection) -> (u64, u128) {
    (
        selection.session.started_at,
        std::fs::metadata(&selection.jsonl_path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or(0),
    )
}

fn sample_process_metrics(pid: u32) -> Option<ProcessMetrics> {
    let sys_pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[sys_pid]),
        false,
        ProcessRefreshKind::nothing()
            .with_cpu()
            .with_memory()
            .with_disk_usage(),
    );

    let timestamp = Utc::now();
    let process = system.process(sys_pid)?;
    let disk_usage = process.disk_usage();
    Some(ProcessMetrics {
        timestamp,
        process_alive: process.exists(),
        cpu_percent: process.cpu_usage(),
        rss_bytes: process.memory(),
        virtual_bytes: process.virtual_memory(),
        io_read_bytes: disk_usage.total_read_bytes,
        io_write_bytes: disk_usage.total_written_bytes,
        io_read_rate: 0.0,
        io_write_rate: 0.0,
    })
}

fn session_jsonl_path(home: &Path, session: &SessionFile) -> Result<PathBuf> {
    let slug = path_to_slug(Path::new(&session.cwd));
    let path = home.join(format!(
        ".claude/projects/{}/{}.jsonl",
        slug, session.session_id
    ));

    if path.exists() {
        Ok(path)
    } else {
        bail!(
            "session {} for PID {} does not have a jsonl log at {}",
            session.session_id,
            session.pid,
            path.display()
        )
    }
}

fn path_to_slug(path: &Path) -> String {
    path.to_string_lossy().replace(['/', '\\', ':'], "-")
}

fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn paths_match(left: &Path, right: &Path) -> bool {
    normalize_path(left) == normalize_path(right)
}

fn normalize_guard_paths(cwd: &Path, guards: &[PathBuf]) -> Vec<PathBuf> {
    guards
        .iter()
        .map(|guard| {
            let joined = if guard.is_absolute() {
                guard.to_path_buf()
            } else {
                cwd.join(guard)
            };

            joined.canonicalize().unwrap_or(joined)
        })
        .collect()
}

fn current_offset(path: &Path) -> u64 {
    std::fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

fn tailer_options(options: &WatcherRuntimeOptions, process_pid: Option<u32>) -> JsonlTailerOptions {
    JsonlTailerOptions {
        follow_newest: false,
        force_poll: options.force_poll,
        process_pid,
    }
}

#[cfg(test)]
mod tests {
    use super::{NoopRuntimeStateRefresher, WatcherRuntime, WatcherRuntimeOptions};
    use saw_core::{AgentEvent, AgentPhase, ClassifierConfig};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::Arc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use tokio::time::timeout;

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("saw-runtime-{prefix}-{nanos}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_session_fixture(
        home: &Path,
        project: &Path,
        pid: u32,
        session_id: &str,
        started_at: u64,
    ) -> PathBuf {
        let sessions_dir = home.join(".claude/sessions");
        fs::create_dir_all(&sessions_dir).unwrap();
        fs::write(
            sessions_dir.join(format!("{session_id}.json")),
            serde_json::json!({
                "pid": pid,
                "sessionId": session_id,
                "cwd": project.display().to_string(),
                "startedAt": started_at
            })
            .to_string(),
        )
        .unwrap();

        let slug = project.display().to_string().replace(['/', '\\', ':'], "-");
        let project_dir = home.join(".claude/projects").join(slug);
        fs::create_dir_all(&project_dir).unwrap();
        let jsonl = project_dir.join(format!("{session_id}.jsonl"));
        fs::write(
            &jsonl,
            format!(
                "{{\"ts\":\"2026-03-24T05:41:20.047714044Z\",\"kind\":\"session_started\",\"type\":\"session_started\",\"sessionId\":\"{session_id}\"}}\n"
            ),
        )
        .unwrap();
        jsonl
    }

    fn spawn_sleep_process() -> Child {
        #[cfg(unix)]
        {
            Command::new("sleep")
                .arg("30")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        }

        #[cfg(windows)]
        {
            Command::new("cmd")
                .args(["/C", "ping -n 30 127.0.0.1 >NUL"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap()
        }
    }

    fn terminate(child: &mut Child) {
        let _ = child.kill();
        let _ = child.wait();
    }

    #[tokio::test]
    #[cfg_attr(windows, ignore = "windows session discovery is unstable in CI")]
    async fn attaches_to_latest_session_for_requested_project() {
        let home = unique_temp_dir("attach-home");
        let project = unique_temp_dir("attach-project");
        let other = unique_temp_dir("attach-other");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&other).unwrap();
        let mut older = spawn_sleep_process();
        let mut newer = spawn_sleep_process();
        let mut other_child = spawn_sleep_process();

        write_session_fixture(&home, &project, older.id(), "ses-older", 10);
        let expected_jsonl = write_session_fixture(&home, &project, newer.id(), "ses-newer", 20);
        write_session_fixture(&home, &other, other_child.id(), "ses-other", 30);
        fs::write(&expected_jsonl, "newer\nwith more activity").unwrap();

        let mut runtime = WatcherRuntime::attach(
            WatcherRuntimeOptions {
                home_dir: home.clone(),
                cwd: project.clone(),
                file: None,
                pid: None,
                session_id: None,
                guard_paths: Vec::new(),
                force_poll: true,
                classifier_config: ClassifierConfig::default(),
            },
            Arc::new(NoopRuntimeStateRefresher),
        )
        .unwrap();

        assert_eq!(runtime.target().pid, Some(newer.id()));
        assert_eq!(runtime.target().jsonl_path, expected_jsonl);

        runtime.shutdown().await;
        terminate(&mut older);
        terminate(&mut newer);
        terminate(&mut other_child);
        fs::remove_dir_all(home).unwrap();
        fs::remove_dir_all(project).unwrap();
        fs::remove_dir_all(other).unwrap();
    }

    #[tokio::test]
    async fn shutdown_detaches_runtime_sources() {
        let home = unique_temp_dir("detach-home");
        let project = unique_temp_dir("detach-project");
        let log_dir = unique_temp_dir("detach-log");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&log_dir).unwrap();
        let jsonl = log_dir.join("session.jsonl");
        fs::write(&jsonl, "").unwrap();

        let mut runtime = WatcherRuntime::attach(
            WatcherRuntimeOptions {
                home_dir: home.clone(),
                cwd: project.clone(),
                file: Some(jsonl),
                pid: None,
                session_id: Some("ses-detach".into()),
                guard_paths: Vec::new(),
                force_poll: true,
                classifier_config: ClassifierConfig::default(),
            },
            Arc::new(NoopRuntimeStateRefresher),
        )
        .unwrap();

        runtime.shutdown().await;

        assert!(runtime.is_shutdown());
        assert!(runtime.next_update().await.unwrap().is_none());

        fs::remove_dir_all(home).unwrap();
        fs::remove_dir_all(project).unwrap();
        fs::remove_dir_all(log_dir).unwrap();
    }

    #[tokio::test]
    async fn normalizes_appended_jsonl_events_into_runtime_updates() {
        let home = unique_temp_dir("events-home");
        let project = unique_temp_dir("events-project");
        let log_dir = unique_temp_dir("events-log");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&log_dir).unwrap();
        let jsonl = log_dir.join("session.jsonl");
        fs::write(
            &jsonl,
            "{\"ts\":\"2026-03-24T05:41:20.047714044Z\",\"kind\":\"session_started\",\"type\":\"session_started\",\"sessionId\":\"ses-events\"}\n",
        )
        .unwrap();

        let mut runtime = WatcherRuntime::attach(
            WatcherRuntimeOptions {
                home_dir: home.clone(),
                cwd: project.clone(),
                file: Some(jsonl.clone()),
                pid: None,
                session_id: Some("ses-events".into()),
                guard_paths: Vec::new(),
                force_poll: true,
                classifier_config: ClassifierConfig::default(),
            },
            Arc::new(NoopRuntimeStateRefresher),
        )
        .unwrap();

        fs::write(
            &jsonl,
            concat!(
                "{\"ts\":\"2026-03-24T05:41:20.047714044Z\",\"kind\":\"session_started\",\"type\":\"session_started\",\"sessionId\":\"ses-events\"}\n",
                "{\"type\":\"assistant\",\"timestamp\":\"2026-03-24T12:11:40.700Z\",\"message\":{\"stop_reason\":\"tool_use\",\"usage\":{\"input_tokens\":123,\"output_tokens\":45},\"content\":[{\"type\":\"tool_use\",\"name\":\"Bash\",\"input\":{\"command\":\"cargo test -p saw-core\"}}]}}\n"
            ),
        )
        .unwrap();

        let update = timeout(Duration::from_secs(5), async {
            loop {
                if let Some(update) = runtime.next_update().await.unwrap() {
                    if matches!(update.event, Some(AgentEvent::ToolCall(_))) {
                        break update;
                    }
                }
            }
        })
        .await
        .expect("runtime update should arrive");

        assert!(matches!(update.phase, AgentPhase::Working));
        assert!(matches!(update.event, Some(AgentEvent::ToolCall(_))));
        assert_eq!(update.state.last_tool_name.as_deref(), Some("Bash"));
        assert_eq!(update.state.session_id.as_deref(), Some("ses-events"));

        runtime.shutdown().await;
        fs::remove_dir_all(home).unwrap();
        fs::remove_dir_all(project).unwrap();
        fs::remove_dir_all(log_dir).unwrap();
    }
}
