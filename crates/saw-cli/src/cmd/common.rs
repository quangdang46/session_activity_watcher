use anyhow::{bail, Context, Result};
use chrono::{SecondsFormat, Utc};
use nix::{
    sys::signal::{kill, Signal},
    unistd::Pid as UnixPid,
};
use saw_core::{
    classify, compute_silence, AgentEvent, AgentPhase, AgentState, ClassifierConfig,
    FileModification, ProcessMetrics, SessionRecord, TaskFile,
};
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::collections::{hash_map::DefaultHasher, HashMap};
use std::ffi::OsStr;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{self, BufRead, BufReader, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

pub const DEFAULT_IDLE_SECS: u64 = 600;

static SESSION_METADATA_CACHE: OnceLock<Mutex<HashMap<String, CachedSessionMetadata>>> =
    OnceLock::new();

#[cfg(test)]
pub fn home_env_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static HOME_ENV_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    HOME_ENV_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("home env test lock poisoned")
}

#[derive(Debug, Clone, Default)]
struct CachedSessionMetadata {
    jsonl_len: u64,
    team_name: Option<String>,
    agent_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub cwd: PathBuf,
    pub session_file: Option<PathBuf>,
    pub pid: Option<u32>,
    pub state: AgentState,
    pub phase: AgentPhase,
    pub silence: Duration,
}

#[derive(Debug, Clone)]
pub struct SessionSelection {
    pub pid: u32,
    pub session: SessionFile,
    pub jsonl_path: PathBuf,
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
        PathBuf::from(&self.cwd)
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(&self.cwd))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RecentFileEntry {
    pub path: String,
    pub kind: String,
    pub age_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusJson {
    pub cwd: String,
    pub pid: Option<u32>,
    pub session_id: Option<String>,
    pub session_file: Option<String>,
    pub phase: &'static str,
    pub last_file: Option<String>,
    pub silence_secs: u64,
    pub process_alive: bool,
    pub cpu_percent: f32,
    pub rss_bytes: u64,
    pub virtual_bytes: u64,
    pub io_read_bytes: u64,
    pub io_write_bytes: u64,
    pub compact_count: u64,
    pub last_tool: Option<String>,
    pub last_command: Option<String>,
    pub guard_paths: Vec<String>,
    pub recent_files: Vec<RecentFileEntry>,
    pub state: AgentState,
}

pub fn snapshot_json(snapshot: &Snapshot) -> StatusJson {
    let now = Utc::now();
    StatusJson {
        cwd: snapshot.cwd.display().to_string(),
        pid: snapshot.pid,
        session_id: snapshot.state.session_id.clone(),
        session_file: snapshot
            .session_file
            .as_ref()
            .map(|path| path.display().to_string()),
        phase: phase_label(&snapshot.phase),
        last_file: snapshot
            .state
            .last_file_path
            .as_ref()
            .map(|path| display_path(&snapshot.cwd, path)),
        silence_secs: snapshot.silence.as_secs(),
        process_alive: snapshot.state.process_alive,
        cpu_percent: snapshot.state.latest_cpu_percent,
        rss_bytes: snapshot.state.latest_rss_bytes,
        virtual_bytes: snapshot.state.latest_virtual_bytes,
        io_read_bytes: snapshot.state.latest_io_read_bytes,
        io_write_bytes: snapshot.state.latest_io_write_bytes,
        compact_count: snapshot.state.compact_count,
        last_tool: snapshot.state.last_tool_name.clone(),
        last_command: snapshot.state.last_command.clone(),
        guard_paths: snapshot
            .state
            .guard_paths
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        recent_files: snapshot
            .state
            .recently_modified_files
            .iter()
            .rev()
            .take(8)
            .map(|file| RecentFileEntry {
                path: display_path(&snapshot.cwd, &file.path),
                kind: format!("{:?}", file.kind),
                age_secs: now
                    .signed_duration_since(file.timestamp)
                    .to_std()
                    .unwrap_or_default()
                    .as_secs(),
            })
            .collect(),
        state: snapshot.state.clone(),
    }
}

pub fn collect_snapshot(
    file: Option<&PathBuf>,
    dir: &Path,
    timeout_secs: u64,
    guards: &[PathBuf],
    session_id: Option<&str>,
    show_all: bool,
) -> Result<Snapshot> {
    let cwd = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let home = home_dir()?;
    let live_session = if show_all {
        None
    } else if let Some(session_id) = session_id {
        resolve_session_selection(&home, Some(&cwd), None, Some(session_id))?
    } else if file.is_none() {
        resolve_session_selection(&home, Some(&cwd), None, None)?
            .filter(|selection| selection.session.cwd_path() == cwd)
    } else {
        None
    };
    let pid = live_session.as_ref().map(|selection| selection.pid);

    let session_file = match file {
        Some(path) => Some(path.clone()),
        None => live_session
            .as_ref()
            .map(|selection| selection.jsonl_path.clone()),
    };

    let normalized_guards = normalize_guard_paths(&cwd, guards);
    let mut state = match session_file.as_ref() {
        Some(path) => load_state(path, &normalized_guards)?,
        None => AgentState {
            guard_paths: normalized_guards.clone(),
            ..Default::default()
        },
    };

    if state.session_id.is_none() {
        state.session_id = live_session
            .as_ref()
            .map(|selection| selection.session.session_id.clone());
    }
    state.session_jsonl_path = session_file.clone();

    if let Some(session_id) = state.session_id.clone() {
        load_hook_events(&mut state, &cwd, &session_id)?;
    }
    refresh_task_context(&home, &mut state)?;

    match pid.and_then(sample_process_metrics) {
        Some(metrics) => state.apply(&AgentEvent::ProcessMetrics(metrics)),
        None => state.process_alive = false,
    }

    let now = Utc::now();
    let phase = classify(
        &state,
        now,
        ClassifierConfig {
            thinking_after: Duration::from_secs(45),
            api_hang_after: Duration::from_secs(timeout_secs),
            idle_after: Duration::from_secs(DEFAULT_IDLE_SECS),
            ..ClassifierConfig::default()
        },
    );
    let silence = compute_silence(&state, now);
    state.phase = phase.clone();

    Ok(Snapshot {
        cwd,
        session_file,
        pid,
        state,
        phase,
        silence,
    })
}

pub fn load_state(path: &Path, guards: &[PathBuf]) -> Result<AgentState> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut state = AgentState {
        guard_paths: guards.to_vec(),
        ..Default::default()
    };

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        if let Some(event) = SessionRecord::parse(&line) {
            state.apply(&event);
        }
    }

    if state.last_event_at.is_none() {
        bail!("no parseable session events in {}", path.display());
    }

    Ok(state)
}

pub fn refresh_task_context(home: &Path, state: &mut AgentState) -> Result<()> {
    let Some(task_list_id) = resolve_task_list_id(home, state)? else {
        state.task_list_id = None;
        state.task_files.clear();
        state.task_files_signature = None;
        state.current_task = None;
        return Ok(());
    };

    let task_dir = home.join(".claude/tasks").join(&task_list_id);
    if !task_dir.is_dir() {
        state.task_list_id = Some(task_list_id);
        state.task_files.clear();
        state.task_files_signature = None;
        state.current_task = None;
        return Ok(());
    }

    let signature = task_dir_signature(&task_dir)?;
    let metadata = resolve_session_metadata(home, state)?;
    if state.task_list_id.as_deref() == Some(task_list_id.as_str())
        && state.task_files_signature == Some(signature)
    {
        state.current_task = resolve_current_task(&state.task_files, &metadata);
        return Ok(());
    }

    let task_files = load_task_files(&task_dir)?;
    let current_task = resolve_current_task(&task_files, &metadata);

    state.task_list_id = Some(task_list_id);
    state.task_files = task_files;
    state.task_files_signature = Some(signature);
    state.current_task = current_task;
    Ok(())
}

fn resolve_task_list_id(home: &Path, state: &AgentState) -> Result<Option<String>> {
    let Some(session_id) = state.session_id.as_ref() else {
        return Ok(None);
    };

    let direct_dir = home.join(".claude/tasks").join(session_id);
    if direct_dir.is_dir() {
        return Ok(Some(session_id.clone()));
    }

    let metadata = resolve_session_metadata(home, state)?;
    if let Some(team_name) = metadata.team_name {
        let team_dir = home.join(".claude/tasks").join(&team_name);
        if team_dir.is_dir() {
            return Ok(Some(team_name));
        }
    }

    Ok(None)
}

fn resolve_session_metadata(home: &Path, state: &AgentState) -> Result<CachedSessionMetadata> {
    let Some(session_id) = state.session_id.as_ref() else {
        return Ok(CachedSessionMetadata::default());
    };
    let Some(jsonl_path) = state.session_jsonl_path.as_ref() else {
        return Ok(CachedSessionMetadata::default());
    };

    let jsonl_len = std::fs::metadata(jsonl_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);

    if let Some(cached) = SESSION_METADATA_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .ok()
        .and_then(|cache| cache.get(session_id).cloned())
        .filter(|cached| cached.jsonl_len == jsonl_len)
    {
        return Ok(cached);
    }

    let metadata = scan_session_metadata(home, jsonl_path, session_id)?;
    if let Ok(mut cache) = SESSION_METADATA_CACHE
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
    {
        cache.insert(session_id.clone(), metadata.clone());
    }
    Ok(metadata)
}

fn scan_session_metadata(
    home: &Path,
    jsonl_path: &Path,
    session_id: &str,
) -> Result<CachedSessionMetadata> {
    let file = File::open(jsonl_path)
        .with_context(|| format!("failed to open {}", jsonl_path.display()))?;
    let reader = BufReader::new(file);
    let mut metadata = CachedSessionMetadata {
        jsonl_len: std::fs::metadata(jsonl_path)
            .map(|value| value.len())
            .unwrap_or(0),
        ..CachedSessionMetadata::default()
    };

    for line in reader.lines() {
        let line = match line {
            Ok(line) => line,
            Err(_) => continue,
        };
        let record: serde_json::Value = match serde_json::from_str(&line) {
            Ok(record) => record,
            Err(_) => continue,
        };
        if record.get("sessionId").and_then(|value| value.as_str()) != Some(session_id) {
            continue;
        }

        if metadata.team_name.is_none() {
            metadata.team_name = record
                .get("teamName")
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned);
        }
        if metadata.agent_name.is_none() {
            metadata.agent_name = record
                .get("agentName")
                .and_then(|value| value.as_str())
                .map(ToOwned::to_owned);
        }

        if metadata.team_name.is_some() && metadata.agent_name.is_some() {
            break;
        }
    }

    if metadata.team_name.is_none() {
        metadata.team_name =
            infer_team_name_from_agent(home, session_id, metadata.agent_name.as_deref())?;
    }

    Ok(metadata)
}

fn infer_team_name_from_agent(
    home: &Path,
    session_id: &str,
    agent_name: Option<&str>,
) -> Result<Option<String>> {
    let team_dir = home.join(".claude/teams");
    if !team_dir.is_dir() {
        return Ok(None);
    }

    for entry in std::fs::read_dir(&team_dir)
        .with_context(|| format!("failed to read {}", team_dir.display()))?
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let config_path = entry.path().join("config.json");
        let raw = match std::fs::read_to_string(&config_path) {
            Ok(raw) => raw,
            Err(_) => continue,
        };
        let config: TeamConfig = match serde_json::from_str(&raw) {
            Ok(config) => config,
            Err(_) => continue,
        };
        if config.lead_session_id == session_id {
            return Ok(Some(config.name));
        }
        if let Some(agent_name) = agent_name {
            if config
                .members
                .iter()
                .any(|member| member.name == agent_name)
            {
                return Ok(Some(config.name));
            }
        }
    }

    Ok(None)
}

#[derive(Debug, Deserialize)]
struct TeamConfig {
    name: String,
    #[serde(rename = "leadSessionId")]
    lead_session_id: String,
    members: Vec<TeamMember>,
}

#[derive(Debug, Deserialize)]
struct TeamMember {
    name: String,
}

fn resolve_current_task(
    task_files: &HashMap<String, TaskFile>,
    metadata: &CachedSessionMetadata,
) -> Option<TaskFile> {
    let mut in_progress = task_files
        .values()
        .filter(|task| task.status == "in_progress")
        .cloned()
        .collect::<Vec<_>>();

    if in_progress.len() == 1 {
        return in_progress.pop();
    }

    let agent_name = metadata.agent_name.as_deref()?;

    let mut owned = in_progress
        .into_iter()
        .filter(|task| task.owner.as_deref() == Some(agent_name))
        .collect::<Vec<_>>();
    if owned.len() == 1 {
        owned.pop()
    } else {
        None
    }
}

fn load_task_files(task_dir: &Path) -> Result<HashMap<String, TaskFile>> {
    let mut task_files = HashMap::new();

    for entry in std::fs::read_dir(task_dir)
        .with_context(|| format!("failed to read {}", task_dir.display()))?
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        if path.extension() != Some(OsStr::new("json")) {
            continue;
        }

        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(_) => continue,
        };
        let task: TaskFile = match serde_json::from_str(&raw) {
            Ok(task) => task,
            Err(_) => continue,
        };
        task_files.insert(task.id.clone(), task);
    }

    Ok(task_files)
}

fn task_dir_signature(task_dir: &Path) -> Result<u64> {
    let mut entries = std::fs::read_dir(task_dir)
        .with_context(|| format!("failed to read {}", task_dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension() == Some(OsStr::new("json")))
        .collect::<Vec<_>>();
    entries.sort();

    let mut hasher = DefaultHasher::new();
    for path in entries {
        path.hash(&mut hasher);
        if let Ok(metadata) = std::fs::metadata(&path) {
            metadata.len().hash(&mut hasher);
            if let Ok(modified) = metadata.modified() {
                if let Ok(duration) = modified.duration_since(std::time::UNIX_EPOCH) {
                    duration.as_nanos().hash(&mut hasher);
                }
            }
        }
    }

    Ok(hasher.finish())
}

pub fn hook_log_path(project_dir: &Path, session_id: &str) -> PathBuf {
    project_dir
        .join(".saw/hooks")
        .join(format!("{session_id}.jsonl"))
}

pub fn append_hook_event(
    project_dir: &Path,
    session_id: &str,
    event: &AgentEvent,
) -> Result<PathBuf> {
    let path = hook_log_path(project_dir, session_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    serde_json::to_writer(&mut file, event)?;
    file.write_all(b"\n")?;

    Ok(path)
}

pub fn load_hook_events(
    state: &mut AgentState,
    project_dir: &Path,
    session_id: &str,
) -> Result<()> {
    let path = hook_log_path(project_dir, session_id);
    if !path.exists() {
        return Ok(());
    }

    let file = File::open(&path).with_context(|| format!("failed to open {}", path.display()))?;
    let reader = BufReader::new(file);
    let cutoff = state.last_jsonl_record_at;

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let event: AgentEvent = match serde_json::from_str(&line) {
            Ok(event) => event,
            Err(_) => continue,
        };

        if cutoff
            .map(|cutoff| event.timestamp() <= cutoff)
            .unwrap_or(false)
        {
            continue;
        }

        state.last_hook_event_at = Some(
            state
                .last_hook_event_at
                .map(|current| current.max(event.timestamp()))
                .unwrap_or_else(|| event.timestamp()),
        );
        state.apply(&event);
    }

    Ok(())
}

pub fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set; cannot inspect ~/.claude/sessions")
}

#[allow(dead_code)]
pub fn find_claude_session(home: &Path, cwd: Option<&Path>) -> Result<(u32, SessionFile)> {
    resolve_session_selection(home, cwd, None, None)?
        .map(|selection| (selection.pid, selection.session))
        .context(match cwd {
            Some(cwd) => format!(
                "no running Claude Code sessions found in ~/.claude/sessions for {}",
                cwd.display()
            ),
            None => "no running Claude Code sessions found in ~/.claude/sessions".to_string(),
        })
}

#[cfg(test)]
pub fn choose_session(
    sessions: &[(u32, SessionFile)],
    cwd: Option<&Path>,
) -> Option<(u32, SessionFile)> {
    let matching = cwd.map(|preferred| {
        sessions
            .iter()
            .filter(|(_, session)| session.cwd_path() == preferred)
            .cloned()
            .collect::<Vec<_>>()
    });

    let candidates: &[(u32, SessionFile)] = matching
        .as_deref()
        .filter(|matches| !matches.is_empty())
        .unwrap_or(sessions);

    candidates
        .iter()
        .max_by_key(|(_, session)| session.started_at)
        .cloned()
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
    let preferred_cwd = cwd.map(|path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf()));

    match preferred_cwd.as_deref() {
        Some(preferred) => sessions
            .iter()
            .filter(|selection| selection.session.cwd_path() == preferred)
            .cloned()
            .collect(),
        None => sessions.to_vec(),
    }
}

pub fn resolve_session_selection(
    home: &Path,
    cwd: Option<&Path>,
    pid: Option<u32>,
    session_id: Option<&str>,
) -> Result<Option<SessionSelection>> {
    let sessions = list_alive_session_selections(home)?;

    if let Some(pid) = pid {
        return Ok(sessions.into_iter().find(|selection| selection.pid == pid));
    }

    if let Some(session_id) = session_id {
        return Ok(sessions
            .into_iter()
            .find(|selection| selection.session.session_id == session_id));
    }

    Ok(sessions_for_cwd(&sessions, cwd).into_iter().next())
}

pub fn prompt_for_session_selection(
    sessions: &[SessionSelection],
    cwd: &Path,
) -> Result<Option<SessionSelection>> {
    if sessions.len() <= 1 || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Ok(sessions.first().cloned());
    }

    let mut stdout = io::stdout().lock();
    writeln!(
        stdout,
        "Multiple Claude sessions found for {}:",
        cwd.display()
    )?;
    for (index, selection) in sessions.iter().enumerate() {
        writeln!(
            stdout,
            "  [{}] {}  pid={}  started={}  source={}",
            index + 1,
            selection.session.session_id,
            selection.pid,
            selection.session.started_at,
            selection.jsonl_path.display(),
        )?;
    }
    write!(
        stdout,
        "Select a session [1-{}] (default 1): ",
        sessions.len()
    )?;
    stdout.flush()?;
    drop(stdout);

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(sessions.first().cloned());
    }

    let index = trimmed
        .parse::<usize>()
        .ok()
        .filter(|index| *index >= 1 && *index <= sessions.len())
        .context("invalid session selection")?;

    Ok(sessions.get(index - 1).cloned())
}

pub fn list_alive_sessions(home: &Path) -> Result<Vec<(u32, SessionFile)>> {
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

pub fn is_pid_alive(system: &mut System, pid: u32) -> bool {
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

pub fn sample_process_metrics(pid: u32) -> Option<ProcessMetrics> {
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

pub fn session_jsonl_path(home: &Path, session: &SessionFile) -> Result<PathBuf> {
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

pub fn path_to_slug(path: &Path) -> String {
    path.to_string_lossy().replace('/', "-")
}

pub fn normalize_guard_paths(cwd: &Path, guards: &[PathBuf]) -> Vec<PathBuf> {
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

pub fn interrupt_pid(pid: u32) -> Result<()> {
    signal_pid(pid, Signal::SIGINT, "INT")
}

pub fn force_kill_pid(pid: u32) -> Result<()> {
    signal_pid(pid, Signal::SIGKILL, "KILL")
}

fn signal_pid(pid: u32, signal: Signal, signal_name: &str) -> Result<()> {
    let raw_pid = i32::try_from(pid).context("pid does not fit in platform pid_t")?;
    kill(UnixPid::from_raw(raw_pid), signal)
        .with_context(|| format!("failed to send SIG{signal_name} to pid {pid}"))
}

pub fn save_checkpoint(state: &AgentState, cwd: &Path) -> Result<PathBuf> {
    let created_at = Utc::now();
    let ts = created_at.format("%Y%m%d-%H%M%S").to_string();
    let checkpoint_dir = cwd.join(".saw/checkpoints").join(ts);
    std::fs::create_dir_all(&checkpoint_dir)
        .with_context(|| format!("failed to create {}", checkpoint_dir.display()))?;

    let files = checkpoint_files(state, cwd);
    let mut manifest = Vec::with_capacity(files.len());
    for (source, relative) in &files {
        let dest = checkpoint_dir.join(relative);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(source, &dest).with_context(|| {
            format!(
                "failed to copy checkpoint file {} to {}",
                source.display(),
                dest.display()
            )
        })?;
        manifest.push(relative.display().to_string());
    }

    let state_json = serde_json::to_string_pretty(state)?;
    std::fs::write(checkpoint_dir.join("saw-state.json"), state_json)?;

    if let Some(jsonl_path) = &state.session_jsonl_path {
        if jsonl_path.exists() {
            std::fs::copy(jsonl_path, checkpoint_dir.join("session-snapshot.jsonl"))
                .with_context(|| format!("failed to copy {}", jsonl_path.display()))?;
        }
    }

    let manifest_json = serde_json::to_string_pretty(&serde_json::json!({
        "created_at": created_at.to_rfc3339_opts(SecondsFormat::Secs, true),
        "files": manifest,
    }))?;
    std::fs::write(checkpoint_dir.join("manifest.json"), manifest_json)?;

    println!("[saw] Checkpoint saved -> {}", checkpoint_dir.display());

    Ok(checkpoint_dir)
}

fn checkpoint_files(state: &AgentState, cwd: &Path) -> Vec<(PathBuf, PathBuf)> {
    let mut files = Vec::new();

    for FileModification { path, .. } in &state.recently_modified_files {
        if !path.exists() || !path.starts_with(cwd) {
            continue;
        }

        let Ok(relative) = path.strip_prefix(cwd) else {
            continue;
        };

        if files
            .iter()
            .any(|(_, existing_relative)| existing_relative == relative)
        {
            continue;
        }

        files.push((path.clone(), relative.to_path_buf()));
    }

    files
}

pub fn display_path(cwd: &Path, path: &Path) -> String {
    path.strip_prefix(cwd)
        .map(|relative| relative.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

pub fn phase_label(phase: &AgentPhase) -> &'static str {
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

pub fn status_exit_code(phase: &AgentPhase) -> i32 {
    match phase {
        AgentPhase::ApiHang(_) | AgentPhase::Dead => 1,
        AgentPhase::ToolLoop { .. } | AgentPhase::TestLoop { .. } => 2,
        AgentPhase::ScopeLeaking { .. } => 3,
        AgentPhase::Idle(_) => 4,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::field_reassign_with_default)]
    use super::{
        append_hook_event, choose_session, collect_snapshot, hook_log_path,
        list_alive_session_selections, load_hook_events, normalize_guard_paths, path_to_slug,
        phase_label, refresh_task_context, save_checkpoint, session_activity_key,
        session_jsonl_path, snapshot_json, status_exit_code, AgentEvent, AgentPhase, SessionFile,
        Snapshot,
    };
    use chrono::Utc;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn picks_single_session_when_only_one_exists() {
        let project = unique_temp_dir("single-project");
        let sessions = vec![(
            1,
            SessionFile {
                pid: 1,
                session_id: "only".into(),
                cwd: project.display().to_string(),
                started_at: 10,
                session_file_path: None,
            },
        )];

        let (_, session) = choose_session(&sessions, Some(&project)).unwrap();
        assert_eq!(session.session_id, "only");
    }

    #[test]
    fn picks_most_recent_matching_cwd() {
        let project = unique_temp_dir("project");
        let other = unique_temp_dir("other");
        let sessions = vec![
            (
                1,
                SessionFile {
                    pid: 1,
                    session_id: "older".into(),
                    cwd: project.display().to_string(),
                    started_at: 10,
                    session_file_path: None,
                },
            ),
            (
                2,
                SessionFile {
                    pid: 2,
                    session_id: "newer".into(),
                    cwd: project.display().to_string(),
                    started_at: 20,
                    session_file_path: None,
                },
            ),
            (
                3,
                SessionFile {
                    pid: 3,
                    session_id: "other-project".into(),
                    cwd: other.display().to_string(),
                    started_at: 30,
                    session_file_path: None,
                },
            ),
        ];

        let (_, session) = choose_session(&sessions, Some(&project)).unwrap();
        assert_eq!(session.session_id, "newer");
    }

    #[test]
    fn returns_none_when_candidate_list_is_empty() {
        assert!(choose_session(&[], None).is_none());
    }

    #[test]
    fn lists_alive_sessions_sorted_by_recency_key() {
        let home = unique_temp_dir("alive-sessions-home");
        let project = unique_temp_dir("alive-sessions-project");
        let mut older = spawn_sleep_process();
        let mut newer = spawn_sleep_process();
        let older_jsonl = write_session_fixture(&home, &project, older.id(), "ses-older", 10);
        let newer_jsonl = write_session_fixture(&home, &project, newer.id(), "ses-newer", 20);

        let sessions = list_alive_session_selections(&home).unwrap();

        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session.session_id, "ses-newer");
        assert_eq!(sessions[0].jsonl_path, newer_jsonl);
        assert_eq!(sessions[1].session.session_id, "ses-older");
        assert_eq!(sessions[1].jsonl_path, older_jsonl);
        assert!(sessions[0].session.session_file_path.is_some());
        assert!(session_activity_key(&sessions[0]) > session_activity_key(&sessions[1]));

        terminate(&mut older);
        terminate(&mut newer);
    }

    #[test]
    fn builds_jsonl_path_from_session_file() {
        let home = unique_temp_dir("jsonl");
        let project_dir = unique_temp_dir("watcher");
        let slug = path_to_slug(&project_dir);
        let project_root = home.join(".claude/projects").join(&slug);
        fs::create_dir_all(&project_root).unwrap();
        let jsonl = project_root.join("ses-123.jsonl");
        fs::write(&jsonl, "{}").unwrap();

        let path = session_jsonl_path(
            &home,
            &SessionFile {
                pid: 1,
                session_id: "ses-123".into(),
                cwd: project_dir.display().to_string(),
                started_at: 1,
                session_file_path: None,
            },
        )
        .unwrap();

        assert_eq!(path, jsonl);
    }

    #[test]
    fn normalize_guard_paths_resolves_relative_paths() {
        let cwd = unique_temp_dir("guard");
        let guard = cwd.join("src/auth");
        fs::create_dir_all(&guard).unwrap();

        assert_eq!(
            normalize_guard_paths(&cwd, &[PathBuf::from("src/auth")]),
            vec![guard]
        );
    }

    #[test]
    fn hook_events_append_and_reload() {
        let cwd = unique_temp_dir("hooks");
        let mut state = saw_core::AgentState::default();
        state.last_jsonl_record_at = Some(Utc::now() - chrono::Duration::seconds(5));
        let event = AgentEvent::ToolResult {
            timestamp: Utc::now(),
            tool_name: Some("Write".into()),
            is_error: false,
            output: Some("ok".into()),
            stderr: None,
            interrupted: false,
            persisted_output_path: None,
            is_sidechain: false,
        };

        let path = append_hook_event(&cwd, "ses-1", &event).unwrap();
        assert_eq!(path, hook_log_path(&cwd, "ses-1"));
        load_hook_events(&mut state, &cwd, "ses-1").unwrap();

        assert_eq!(state.last_tool_name.as_deref(), Some("Write"));
        assert!(state.last_hook_event_at.is_some());
    }

    #[test]
    fn save_checkpoint_copies_recent_files_and_state() {
        let cwd = unique_temp_dir("checkpoint");
        let file = cwd.join("src/lib.rs");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "fn main() {}\n").unwrap();

        let mut state = saw_core::AgentState::default();
        state
            .recently_modified_files
            .push_back(saw_core::FileModification {
                timestamp: chrono::Utc::now(),
                path: file.clone(),
                kind: saw_core::FileChangeKind::Modified,
                line_change: None,
            });
        state.session_jsonl_path = Some(cwd.join("session.jsonl"));
        fs::write(state.session_jsonl_path.as_ref().unwrap(), "{}\n").unwrap();

        let checkpoint = save_checkpoint(&state, &cwd).unwrap();
        assert!(checkpoint.join("src/lib.rs").exists());
        assert!(checkpoint.join("saw-state.json").exists());
        assert!(checkpoint.join("session-snapshot.jsonl").exists());
        assert!(checkpoint.join("manifest.json").exists());
    }

    #[test]
    fn save_checkpoint_deduplicates_recent_files_in_manifest() {
        let cwd = unique_temp_dir("checkpoint-dedup");
        let file = cwd.join("src/lib.rs");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "fn main() {}\n").unwrap();

        let mut state = saw_core::AgentState::default();
        for seconds in [3, 2, 1] {
            state
                .recently_modified_files
                .push_back(saw_core::FileModification {
                    timestamp: chrono::Utc::now() - chrono::Duration::seconds(seconds),
                    path: file.clone(),
                    kind: saw_core::FileChangeKind::Modified,
                    line_change: None,
                });
        }

        let checkpoint = save_checkpoint(&state, &cwd).unwrap();
        let manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(checkpoint.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["files"], serde_json::json!(["src/lib.rs"]));
    }

    #[test]
    fn maps_phase_labels_and_exit_codes() {
        assert_eq!(phase_label(&AgentPhase::Working), "WORKING");
        assert_eq!(status_exit_code(&AgentPhase::Working), 0);
        assert_eq!(status_exit_code(&AgentPhase::Thinking), 0);
        assert_eq!(
            status_exit_code(&AgentPhase::TaskBlocked {
                task_id: "3".into(),
                blocked_by: vec!["2".into()],
            }),
            0
        );
        assert_eq!(
            status_exit_code(&AgentPhase::ApiHang(std::time::Duration::from_secs(130))),
            1
        );
        assert_eq!(status_exit_code(&AgentPhase::Dead), 1);
        assert_eq!(
            status_exit_code(&AgentPhase::ToolLoop {
                file: PathBuf::from("src/lib.rs"),
                count: 3,
                since: Utc::now(),
            }),
            2
        );
        assert_eq!(
            status_exit_code(&AgentPhase::TestLoop {
                command: "cargo test".into(),
                failure_count: 3,
            }),
            2
        );
        assert_eq!(
            status_exit_code(&AgentPhase::ScopeLeaking {
                violating_file: PathBuf::from("src/lib.rs"),
                guard_path: PathBuf::from("src/auth"),
            }),
            3
        );
        assert_eq!(
            status_exit_code(&AgentPhase::Idle(std::time::Duration::from_secs(601))),
            4
        );
    }

    #[test]
    fn collect_snapshot_sets_session_metadata_and_dead_phase_without_live_process() {
        let cwd = unique_temp_dir("status-snapshot");
        let jsonl = cwd.join("session.jsonl");
        let timestamp = Utc::now().to_rfc3339();
        fs::write(
            &jsonl,
            format!(
                "{{\"type\":\"session_started\",\"timestamp\":\"{timestamp}\",\"sessionId\":\"ses-123\"}}\n"
            ),
        )
        .unwrap();

        let snapshot = collect_snapshot(Some(&jsonl), &cwd, 130, &[], None, false).unwrap();

        assert_eq!(snapshot.session_file, Some(jsonl.clone()));
        assert_eq!(snapshot.state.session_id.as_deref(), Some("ses-123"));
        assert_eq!(snapshot.state.session_jsonl_path, Some(jsonl));
        assert!(matches!(snapshot.phase, AgentPhase::Dead));
    }

    #[test]
    fn collect_snapshot_uses_explicit_session_id_when_multiple_sessions_share_cwd() {
        let _lock = super::home_env_test_lock();
        let home = unique_temp_dir("snapshot-session-home");
        let project = unique_temp_dir("snapshot-session-project");
        let mut older = spawn_sleep_process();
        let mut newer = spawn_sleep_process();
        let older_jsonl = write_session_fixture(&home, &project, older.id(), "ses-older", 10);
        let newer_jsonl = write_session_fixture(&home, &project, newer.id(), "ses-newer", 20);
        let original_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);

        let snapshot = collect_snapshot(
            Some(&older_jsonl),
            &project,
            130,
            &[],
            Some("ses-older"),
            false,
        )
        .unwrap();

        assert_eq!(snapshot.pid, Some(older.id()));
        assert_eq!(snapshot.session_file, Some(older_jsonl));
        assert_eq!(snapshot.state.session_id.as_deref(), Some("ses-older"));
        assert_eq!(
            snapshot.state.session_jsonl_path,
            Some(snapshot.session_file.clone().unwrap())
        );
        assert!(snapshot.state.process_alive);
        assert_ne!(snapshot.session_file, Some(newer_jsonl));

        if let Some(home) = original_home {
            std::env::set_var("HOME", home);
        } else {
            std::env::remove_var("HOME");
        }
        terminate(&mut older);
        terminate(&mut newer);
    }

    #[test]
    fn refresh_task_context_detects_direct_session_task_list() {
        let home = unique_temp_dir("task-context-direct-home");
        let task_dir = home.join(".claude/tasks/ses-123");
        fs::create_dir_all(&task_dir).unwrap();
        fs::write(
            task_dir.join("2.json"),
            r#"{"id":"2","status":"completed","owner":null,"blockedBy":[]}"#,
        )
        .unwrap();
        fs::write(
            task_dir.join("3.json"),
            r#"{"id":"3","status":"in_progress","owner":null,"blockedBy":["2"]}"#,
        )
        .unwrap();

        let mut state = saw_core::AgentState::default();
        state.session_id = Some("ses-123".into());
        state.session_jsonl_path = Some(home.join("session.jsonl"));
        fs::write(state.session_jsonl_path.as_ref().unwrap(), "").unwrap();

        refresh_task_context(&home, &mut state).unwrap();

        assert_eq!(state.task_list_id.as_deref(), Some("ses-123"));
        assert_eq!(
            state.current_task.as_ref().map(|task| task.id.as_str()),
            Some("3")
        );
        assert_eq!(state.task_files.len(), 2);
    }

    #[test]
    fn refresh_task_context_detects_team_task_list_for_team_lead() {
        let home = unique_temp_dir("task-context-team-home");
        let team_dir = home.join(".claude/teams/swarm-alpha");
        fs::create_dir_all(&team_dir).unwrap();
        fs::write(
            team_dir.join("config.json"),
            r#"{"name":"swarm-alpha","leadSessionId":"ses-team","members":[{"name":"team-lead"}]}"#,
        )
        .unwrap();

        let task_dir = home.join(".claude/tasks/swarm-alpha");
        fs::create_dir_all(&task_dir).unwrap();
        fs::write(
            task_dir.join("1.json"),
            r#"{"id":"1","status":"in_progress","owner":"team-lead","blockedBy":["2"]}"#,
        )
        .unwrap();
        fs::write(
            task_dir.join("2.json"),
            r#"{"id":"2","status":"pending","owner":"worker-a","blockedBy":[]}"#,
        )
        .unwrap();

        let project_dir = home.join(".claude/projects/repo");
        fs::create_dir_all(&project_dir).unwrap();
        let jsonl = project_dir.join("ses-team.jsonl");
        fs::write(
            &jsonl,
            r#"{"type":"agent-name","agentName":"team-lead","sessionId":"ses-team"}"#,
        )
        .unwrap();

        let mut state = saw_core::AgentState::default();
        state.session_id = Some("ses-team".into());
        state.session_jsonl_path = Some(jsonl);

        refresh_task_context(&home, &mut state).unwrap();

        assert_eq!(state.task_list_id.as_deref(), Some("swarm-alpha"));
        assert_eq!(
            state.current_task.as_ref().map(|task| task.id.as_str()),
            Some("1")
        );
    }

    #[test]
    fn snapshot_json_includes_top_level_fields_and_full_agent_state() {
        let cwd = PathBuf::from("/repo");
        let session_file = cwd.join("session.jsonl");
        let mut state = saw_core::AgentState::default();
        state.session_id = Some("ses-123".into());
        state.last_file_path = Some(cwd.join("src/auth/login.rs"));
        state.last_tool_name = Some("Edit".into());
        state.last_command = Some("cargo test".into());
        state.process_alive = true;
        state.latest_cpu_percent = 12.4;
        state.latest_rss_bytes = 1024;
        state.latest_virtual_bytes = 2048;
        state.latest_io_read_bytes = 128;
        state.latest_io_write_bytes = 256;
        state.compact_count = 2;
        state.last_compact_at = Some(Utc::now() - chrono::Duration::seconds(1));
        state.guard_paths = vec![cwd.join("src/auth")];
        state
            .recently_modified_files
            .push_back(saw_core::FileModification {
                timestamp: Utc::now() - chrono::Duration::seconds(3),
                path: cwd.join("src/auth/login.rs"),
                kind: saw_core::FileChangeKind::Modified,
                line_change: None,
            });

        let snapshot = Snapshot {
            cwd: cwd.clone(),
            session_file: Some(session_file.clone()),
            pid: Some(42),
            state,
            phase: AgentPhase::Working,
            silence: std::time::Duration::from_secs(3),
        };

        let json = snapshot_json(&snapshot);

        assert_eq!(json.cwd, "/repo");
        assert_eq!(json.pid, Some(42));
        assert_eq!(json.session_id.as_deref(), Some("ses-123"));
        assert_eq!(json.session_file.as_deref(), Some("/repo/session.jsonl"));
        assert_eq!(json.phase, "WORKING");
        assert_eq!(json.last_file.as_deref(), Some("src/auth/login.rs"));
        assert_eq!(json.silence_secs, 3);
        assert_eq!(json.cpu_percent, 12.4);
        assert_eq!(json.compact_count, 2);
        assert_eq!(json.last_tool.as_deref(), Some("Edit"));
        assert_eq!(json.last_command.as_deref(), Some("cargo test"));
        assert_eq!(json.state.compact_count, 2);
        assert_eq!(json.guard_paths, vec!["/repo/src/auth".to_string()]);
        assert_eq!(json.recent_files.len(), 1);
        assert_eq!(json.recent_files[0].path, "src/auth/login.rs");
        assert_eq!(json.state.session_id.as_deref(), Some("ses-123"));
        assert_eq!(
            json.state.last_file_path,
            Some(cwd.join("src/auth/login.rs"))
        );
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
            .join(project.to_string_lossy().replace('/', "-"));
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

    fn spawn_sleep_process() -> std::process::Child {
        std::process::Command::new("python3")
            .arg("-c")
            .arg("import time; time.sleep(30)")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }

    fn terminate(child: &mut std::process::Child) {
        let _ = child.kill();
        let _ = child.wait();
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
