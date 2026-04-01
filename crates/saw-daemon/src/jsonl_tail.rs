use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use notify::{
    event::{CreateKind, ModifyKind, RemoveKind},
    Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use saw_core::{AgentEvent, SessionRecord};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use sysinfo::{Pid, ProcessesToUpdate, System};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::mpsc;
use tokio::time::{interval_at, Instant, MissedTickBehavior};

pub const JSONL_TAIL_POLL_INTERVAL: Duration = Duration::from_millis(100);
pub const JSONL_TAIL_NOTIFY_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
pub const JSONL_TAIL_FALLBACK_POLL_INTERVAL: Duration = Duration::from_secs(2);
pub const JSONL_TAIL_SLEEP_WAKE_THRESHOLD: Duration = Duration::from_secs(15);

const INITIAL_RETRY_BACKOFF: Duration = Duration::from_millis(100);
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonlTailMode {
    Notify,
    Polling,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonlTailPollingReason {
    ForcePoll,
    NotifyInactive,
    SleepWakeRecovery,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonlTailRetryReason {
    SourceMissing,
    ActiveFileMissing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonlTailFatalReason {
    Exited,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonlTailConnectionState {
    Healthy,
    Polling {
        reason: JsonlTailPollingReason,
    },
    Retrying {
        reason: JsonlTailRetryReason,
        attempt: u32,
        next_retry_in: Duration,
    },
    SleepWakeRecovery {
        observed_gap: Duration,
    },
    Fatal {
        reason: JsonlTailFatalReason,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonlTailStatus {
    pub observed_at: DateTime<Utc>,
    pub state: JsonlTailConnectionState,
    pub active_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
pub struct JsonlTailerOptions {
    pub follow_newest: bool,
    pub force_poll: bool,
    pub process_pid: Option<u32>,
}

impl Default for JsonlTailerOptions {
    fn default() -> Self {
        Self {
            follow_newest: true,
            force_poll: false,
            process_pid: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileSnapshot {
    path: PathBuf,
    modified: SystemTime,
    len: u64,
}

pub struct JsonlTailer {
    file_path: PathBuf,
    active_path: PathBuf,
    byte_offset: u64,
    line_buffer: Vec<u8>,
    follow_newest: bool,
    watch_root: PathBuf,
    watcher: RecommendedWatcher,
    notify_rx: mpsc::UnboundedReceiver<notify::Result<Event>>,
    next_retry_at: Instant,
    retry_backoff: Duration,
    mode: JsonlTailMode,
    force_poll: bool,
    polling_reason: Option<JsonlTailPollingReason>,
    retry_attempt: u32,
    process_pid: Option<u32>,
    last_notify_event_at: Instant,
    last_loop_tick_at: Instant,
    notify_confirmed: bool,
    last_observed_snapshot: Option<FileSnapshot>,
    last_status_state: Option<(JsonlTailConnectionState, Option<PathBuf>)>,
}

impl JsonlTailer {
    pub fn new(file_path: impl AsRef<Path>) -> notify::Result<Self> {
        Self::with_options(file_path, JsonlTailerOptions::default())
    }

    pub fn with_follow_newest(
        file_path: impl AsRef<Path>,
        follow_newest: bool,
    ) -> notify::Result<Self> {
        Self::with_options(
            file_path,
            JsonlTailerOptions {
                follow_newest,
                ..JsonlTailerOptions::default()
            },
        )
    }

    pub fn with_options(
        file_path: impl AsRef<Path>,
        options: JsonlTailerOptions,
    ) -> notify::Result<Self> {
        let file_path = file_path.as_ref().to_path_buf();
        let watch_root = file_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
            .canonicalize()
            .unwrap_or_else(|_| {
                file_path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("."))
            });
        let (notify_tx, notify_rx) = mpsc::unbounded_channel();
        let mut watcher = RecommendedWatcher::new(
            move |result| {
                let _ = notify_tx.send(result);
            },
            Config::default().with_poll_interval(JSONL_TAIL_POLL_INTERVAL),
        )?;
        watcher.watch(&watch_root, RecursiveMode::NonRecursive)?;

        Ok(Self {
            active_path: file_path.clone(),
            file_path,
            byte_offset: 0,
            line_buffer: Vec::new(),
            follow_newest: options.follow_newest,
            watch_root,
            watcher,
            notify_rx,
            next_retry_at: Instant::now(),
            retry_backoff: INITIAL_RETRY_BACKOFF,
            mode: if options.force_poll {
                JsonlTailMode::Polling
            } else {
                JsonlTailMode::Notify
            },
            force_poll: options.force_poll,
            polling_reason: options
                .force_poll
                .then_some(JsonlTailPollingReason::ForcePoll),
            retry_attempt: 0,
            process_pid: options.process_pid,
            last_notify_event_at: Instant::now(),
            last_loop_tick_at: Instant::now(),
            notify_confirmed: false,
            last_observed_snapshot: None,
            last_status_state: None,
        })
    }

    pub fn file_path(&self) -> &Path {
        &self.file_path
    }

    pub fn active_path(&self) -> &Path {
        &self.active_path
    }

    pub fn byte_offset(&self) -> u64 {
        self.byte_offset
    }

    pub fn set_byte_offset(&mut self, byte_offset: u64) {
        self.byte_offset = byte_offset;
        self.line_buffer.clear();
    }

    pub fn watcher(&self) -> &RecommendedWatcher {
        &self.watcher
    }

    pub fn mode(&self) -> JsonlTailMode {
        self.mode
    }

    pub async fn run(&mut self, sender: mpsc::Sender<AgentEvent>) -> Result<()> {
        self.run_with_status(sender, None).await
    }

    pub async fn run_with_status(
        &mut self,
        sender: mpsc::Sender<AgentEvent>,
        status_sender: Option<mpsc::UnboundedSender<JsonlTailStatus>>,
    ) -> Result<()> {
        let mut poll_ticker = interval_at(
            Instant::now() + JSONL_TAIL_FALLBACK_POLL_INTERVAL,
            JSONL_TAIL_FALLBACK_POLL_INTERVAL,
        );
        poll_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let mut fallback_ticker = interval_at(
            Instant::now() + JSONL_TAIL_NOTIFY_IDLE_TIMEOUT,
            JSONL_TAIL_NOTIFY_IDLE_TIMEOUT,
        );
        fallback_ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        if self.force_poll {
            log::warn!(
                "jsonl tail watcher forcing polling mode for {}",
                self.file_path.display()
            );
        }

        self.sync_once(&sender, status_sender.as_ref()).await?;
        self.refresh_snapshot().await;
        self.publish_operational_status(status_sender.as_ref());

        loop {
            tokio::select! {
                maybe = self.notify_rx.recv() => {
                    self.observe_tick_gap(Instant::now(), status_sender.as_ref());
                    match maybe {
                        Some(Ok(event)) => {
                            if is_relevant_notify_event(&event, &self.watch_root) {
                                self.last_notify_event_at = Instant::now();
                                self.notify_confirmed = true;
                                self.next_retry_at = Instant::now();
                                if matches!(self.mode, JsonlTailMode::Polling) && !self.force_poll {
                                    self.mode = JsonlTailMode::Notify;
                                    self.polling_reason = None;
                                }
                                self.sync_once(&sender, status_sender.as_ref()).await?;
                                self.refresh_snapshot().await;
                            }
                        }
                        Some(Err(error)) => {
                            log::warn!(
                                "jsonl tail watcher error under {}: {error}",
                                self.watch_root.display()
                            );
                        }
                        None => return Ok(()),
                    }
                }
                _ = poll_ticker.tick(), if matches!(self.mode, JsonlTailMode::Polling) => {
                    self.observe_tick_gap(Instant::now(), status_sender.as_ref());
                    self.poll_once(&sender, status_sender.as_ref()).await?;
                }
                _ = fallback_ticker.tick(), if matches!(self.mode, JsonlTailMode::Notify) => {
                    self.observe_tick_gap(Instant::now(), status_sender.as_ref());
                    self.maybe_switch_to_polling(&sender, status_sender.as_ref()).await?;
                }
            }
        }
    }

    async fn maybe_switch_to_polling(
        &mut self,
        sender: &mpsc::Sender<AgentEvent>,
        status_sender: Option<&mpsc::UnboundedSender<JsonlTailStatus>>,
    ) -> Result<()> {
        if !matches!(self.mode, JsonlTailMode::Notify) {
            return Ok(());
        }

        if self.notify_confirmed {
            return Ok(());
        }

        if Instant::now().duration_since(self.last_notify_event_at) < JSONL_TAIL_NOTIFY_IDLE_TIMEOUT
        {
            return Ok(());
        }

        if !self.is_process_alive() {
            return Ok(());
        }

        self.mode = JsonlTailMode::Polling;
        self.polling_reason = Some(JsonlTailPollingReason::NotifyInactive);
        log::warn!(
            "jsonl tail watcher switching to polling for {} after {}s without notify events",
            self.file_path.display(),
            JSONL_TAIL_NOTIFY_IDLE_TIMEOUT.as_secs(),
        );
        self.publish_operational_status(status_sender);
        self.sync_once(sender, status_sender).await?;
        self.refresh_snapshot().await;
        Ok(())
    }

    async fn poll_once(
        &mut self,
        sender: &mpsc::Sender<AgentEvent>,
        status_sender: Option<&mpsc::UnboundedSender<JsonlTailStatus>>,
    ) -> Result<()> {
        let current_snapshot = self.capture_snapshot().await;
        if current_snapshot == self.last_observed_snapshot {
            return Ok(());
        }

        self.sync_once(sender, status_sender).await?;
        self.refresh_snapshot().await;
        Ok(())
    }

    async fn refresh_snapshot(&mut self) {
        self.last_observed_snapshot = self.capture_snapshot().await;
    }

    async fn capture_snapshot(&self) -> Option<FileSnapshot> {
        let path = self.resolve_active_path()?;
        let metadata = tokio::fs::metadata(&path).await.ok()?;
        Some(FileSnapshot {
            path,
            modified: metadata.modified().unwrap_or(UNIX_EPOCH),
            len: metadata.len(),
        })
    }

    fn is_process_alive(&self) -> bool {
        let Some(pid) = self.process_pid else {
            return true;
        };

        let mut system = System::new();
        let pid = Pid::from_u32(pid);
        system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
        system.process(pid).is_some_and(|process| process.exists())
    }

    async fn sync_once(
        &mut self,
        sender: &mpsc::Sender<AgentEvent>,
        status_sender: Option<&mpsc::UnboundedSender<JsonlTailStatus>>,
    ) -> Result<()> {
        let now = Instant::now();
        let active_path = match self.resolve_active_path() {
            Some(path) => path,
            None => {
                if now < self.next_retry_at {
                    return Ok(());
                }
                self.schedule_retry(JsonlTailRetryReason::SourceMissing, status_sender);
                self.byte_offset = 0;
                self.line_buffer.clear();
                self.active_path = self.file_path.clone();
                return Ok(());
            }
        };

        if active_path != self.active_path {
            self.active_path = active_path;
            self.byte_offset = 0;
            self.line_buffer.clear();
        }

        if self.read_available_lines(sender, status_sender).await? {
            self.reset_retry_backoff();
            self.retry_attempt = 0;
            self.publish_operational_status(status_sender);
        }
        Ok(())
    }

    fn resolve_active_path(&self) -> Option<PathBuf> {
        if !self.follow_newest {
            return self.file_path.exists().then(|| self.file_path.clone());
        }

        select_newest_jsonl(&self.watch_root)
    }

    async fn read_available_lines(
        &mut self,
        sender: &mpsc::Sender<AgentEvent>,
        status_sender: Option<&mpsc::UnboundedSender<JsonlTailStatus>>,
    ) -> Result<bool> {
        let metadata = match tokio::fs::metadata(&self.active_path).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.schedule_retry(JsonlTailRetryReason::ActiveFileMissing, status_sender);
                return Ok(false);
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to stat {}", self.active_path.display()));
            }
        };

        let file_len = metadata.len();
        if file_len < self.byte_offset {
            self.byte_offset = 0;
            self.line_buffer.clear();
        }

        if file_len == self.byte_offset {
            return Ok(true);
        }

        let mut file = tokio::fs::File::open(&self.active_path)
            .await
            .with_context(|| format!("failed to open {}", self.active_path.display()))?;
        file.seek(std::io::SeekFrom::Start(self.byte_offset))
            .await
            .with_context(|| format!("failed to seek {}", self.active_path.display()))?;

        let mut bytes = Vec::with_capacity((file_len - self.byte_offset) as usize);
        file.read_to_end(&mut bytes)
            .await
            .with_context(|| format!("failed to read {}", self.active_path.display()))?;
        self.byte_offset += bytes.len() as u64;
        self.line_buffer.extend_from_slice(&bytes);

        let mut consumed = 0usize;
        for index in 0..self.line_buffer.len() {
            if self.line_buffer[index] != b'\n' {
                continue;
            }

            let mut line = self.line_buffer[consumed..index].to_vec();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            consumed = index + 1;

            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }

            let line = match std::str::from_utf8(&line) {
                Ok(line) => line,
                Err(error) => {
                    log::warn!(
                        "jsonl tail ignored non-utf8 line from {}: {error}",
                        self.active_path.display()
                    );
                    continue;
                }
            };

            if let Some(event) = SessionRecord::parse(line) {
                if sender.send(event).await.is_err() {
                    return Ok(true);
                }
            }
        }

        if consumed > 0 {
            self.line_buffer.drain(..consumed);
        }

        Ok(true)
    }

    fn reset_retry_backoff(&mut self) {
        self.next_retry_at = Instant::now();
        self.retry_backoff = INITIAL_RETRY_BACKOFF;
    }

    fn schedule_retry(
        &mut self,
        reason: JsonlTailRetryReason,
        status_sender: Option<&mpsc::UnboundedSender<JsonlTailStatus>>,
    ) {
        self.retry_attempt += 1;
        self.next_retry_at = Instant::now() + self.retry_backoff;
        self.publish_status(
            status_sender,
            JsonlTailConnectionState::Retrying {
                reason,
                attempt: self.retry_attempt,
                next_retry_in: self.retry_backoff,
            },
        );
        self.retry_backoff = std::cmp::min(self.retry_backoff * 2, MAX_RETRY_BACKOFF);
    }

    fn publish_operational_status(
        &mut self,
        status_sender: Option<&mpsc::UnboundedSender<JsonlTailStatus>>,
    ) {
        let state = self
            .polling_reason
            .map(|reason| JsonlTailConnectionState::Polling { reason })
            .unwrap_or(JsonlTailConnectionState::Healthy);
        self.publish_status(status_sender, state);
    }

    fn publish_status(
        &mut self,
        status_sender: Option<&mpsc::UnboundedSender<JsonlTailStatus>>,
        state: JsonlTailConnectionState,
    ) {
        let Some(status_sender) = status_sender else {
            return;
        };
        let active_path = Some(self.active_path.clone());
        let key = (state.clone(), active_path.clone());
        if self.last_status_state.as_ref() == Some(&key) {
            return;
        }

        self.last_status_state = Some(key);
        let _ = status_sender.send(JsonlTailStatus {
            observed_at: Utc::now(),
            state,
            active_path,
        });
    }

    fn observe_tick_gap(
        &mut self,
        now: Instant,
        status_sender: Option<&mpsc::UnboundedSender<JsonlTailStatus>>,
    ) {
        let observed_gap = now.saturating_duration_since(self.last_loop_tick_at);
        self.last_loop_tick_at = now;
        if observed_gap < JSONL_TAIL_SLEEP_WAKE_THRESHOLD {
            return;
        }

        if !self.force_poll {
            self.mode = JsonlTailMode::Polling;
        }
        self.polling_reason = Some(JsonlTailPollingReason::SleepWakeRecovery);
        self.publish_status(
            status_sender,
            JsonlTailConnectionState::SleepWakeRecovery { observed_gap },
        );
    }
}

fn is_relevant_notify_event(event: &Event, watch_root: &Path) -> bool {
    match event.kind {
        EventKind::Modify(ModifyKind::Data(_))
        | EventKind::Create(CreateKind::Any | CreateKind::File)
        | EventKind::Remove(RemoveKind::Any | RemoveKind::File)
        | EventKind::Modify(_) => event.paths.iter().any(|path| {
            path.parent() == Some(watch_root) && path.extension() == Some(OsStr::new("jsonl"))
        }),
        _ => false,
    }
}

fn select_newest_jsonl(dir: &Path) -> Option<PathBuf> {
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    let entries = std::fs::read_dir(dir).ok()?;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension() != Some(OsStr::new("jsonl")) {
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => continue,
        };
        let modified = metadata.modified().unwrap_or(UNIX_EPOCH);

        let should_replace = newest.as_ref().is_none_or(|(best_modified, best_path)| {
            modified > *best_modified || (modified == *best_modified && path > *best_path)
        });
        if should_replace {
            newest = Some((modified, path));
        }
    }

    newest.map(|(_, path)| path)
}

#[cfg(test)]
mod tests {
    use super::{
        FileSnapshot, JsonlTailConnectionState, JsonlTailMode, JsonlTailPollingReason,
        JsonlTailRetryReason, JsonlTailStatus, JsonlTailer, JsonlTailerOptions,
        JSONL_TAIL_FALLBACK_POLL_INTERVAL, JSONL_TAIL_NOTIFY_IDLE_TIMEOUT,
        JSONL_TAIL_POLL_INTERVAL, JSONL_TAIL_SLEEP_WAKE_THRESHOLD,
    };
    use saw_core::AgentEvent;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use tokio::sync::mpsc;
    use tokio::time::{sleep, timeout, Instant};

    #[tokio::test]
    async fn emits_appended_events_without_replaying_existing_bytes() {
        let dir = unique_temp_dir("append");
        let file = dir.join("ses-1.jsonl");
        fs::write(&file, session_started_line("existing")).unwrap();

        let mut tailer = JsonlTailer::with_follow_newest(&file, false).unwrap();
        tailer.set_byte_offset(fs::metadata(&file).unwrap().len());

        let (tx, mut rx) = mpsc::channel(8);
        let handle = tokio::spawn(async move { tailer.run(tx).await.unwrap() });
        sleep(JSONL_TAIL_POLL_INTERVAL).await;

        let started_at = Instant::now();
        append(&file, &session_started_line("fresh"));

        let event = timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(started_at.elapsed() <= Duration::from_millis(750));
        assert_session_start(event, "fresh");

        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn buffers_partial_lines_until_newline_arrives() {
        let dir = unique_temp_dir("partial");
        let file = dir.join("ses-1.jsonl");
        fs::write(&file, "").unwrap();

        let mut tailer = JsonlTailer::with_follow_newest(&file, false).unwrap();
        tailer.set_byte_offset(0);

        let (tx, mut rx) = mpsc::channel(8);
        let handle = tokio::spawn(async move { tailer.run(tx).await.unwrap() });
        sleep(JSONL_TAIL_POLL_INTERVAL).await;

        append(
            &file,
            r#"{"type":"session_started","timestamp":"2026-03-24T12:00:00Z","session_id":"partial"}"#,
        );
        assert!(timeout(Duration::from_millis(300), rx.recv())
            .await
            .is_err());

        append(&file, "\n");
        let event = timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_session_start(event, "partial");

        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn switches_to_newest_jsonl_file_on_rotation() {
        let dir = unique_temp_dir("rotation");
        let first = dir.join("ses-1.jsonl");
        fs::write(&first, "").unwrap();

        let mut tailer = JsonlTailer::new(&first).unwrap();
        tailer.set_byte_offset(0);

        let (tx, mut rx) = mpsc::channel(8);
        let handle = tokio::spawn(async move { tailer.run(tx).await.unwrap() });
        sleep(JSONL_TAIL_POLL_INTERVAL).await;

        append(&first, &session_started_line("first"));
        let event = timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_session_start(event, "first");

        sleep(Duration::from_millis(25)).await;
        let second = dir.join("ses-2.jsonl");
        fs::write(&second, session_started_line("second")).unwrap();

        let event = timeout(Duration::from_secs(2), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_session_start(event, "second");

        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn waits_for_missing_file_and_resumes_when_created() {
        let dir = unique_temp_dir("missing");
        let file = dir.join("ses-missing.jsonl");
        let tailer = JsonlTailer::with_follow_newest(&file, false).unwrap();

        let (tx, mut rx) = mpsc::channel(8);
        let handle = tokio::spawn(async move {
            let mut tailer = tailer;
            tailer.run(tx).await.unwrap()
        });
        sleep(Duration::from_millis(200)).await;

        fs::write(&file, session_started_line("created-later")).unwrap();
        let event = timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_session_start(event, "created-later");

        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn force_poll_starts_in_polling_mode() {
        let dir = unique_temp_dir("force-poll");
        let file = dir.join("ses-1.jsonl");
        fs::write(&file, "").unwrap();

        let mut tailer = JsonlTailer::with_options(
            &file,
            JsonlTailerOptions {
                follow_newest: false,
                force_poll: true,
                process_pid: None,
            },
        )
        .unwrap();
        tailer.set_byte_offset(0);

        assert_eq!(tailer.mode(), JsonlTailMode::Polling);

        let (tx, mut rx) = mpsc::channel(8);
        let handle = tokio::spawn(async move { tailer.run(tx).await.unwrap() });
        sleep(JSONL_TAIL_POLL_INTERVAL).await;
        append(&file, &session_started_line("forced"));

        let event = timeout(Duration::from_secs(3), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_session_start(event, "forced");

        handle.abort();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn switches_to_polling_when_snapshot_changes_without_notify_events() {
        let dir = unique_temp_dir("fallback");
        let file = dir.join("ses-1.jsonl");
        fs::write(&file, session_started_line("fallback")).unwrap();

        let mut tailer = JsonlTailer::with_options(
            &file,
            JsonlTailerOptions {
                follow_newest: false,
                force_poll: false,
                process_pid: Some(std::process::id()),
            },
        )
        .unwrap();
        tailer.last_observed_snapshot = Some(FileSnapshot {
            path: file.clone(),
            modified: UNIX_EPOCH,
            len: 0,
        });
        tailer.last_notify_event_at = Instant::now() - JSONL_TAIL_NOTIFY_IDLE_TIMEOUT;

        let (tx, mut rx) = mpsc::channel(8);
        tailer.maybe_switch_to_polling(&tx, None).await.unwrap();

        assert_eq!(tailer.mode(), JsonlTailMode::Polling);
        let event = timeout(JSONL_TAIL_FALLBACK_POLL_INTERVAL, rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_session_start(event, "fallback");
    }

    #[tokio::test]
    async fn reports_retry_status_with_explicit_backoff_attempts() {
        let dir = unique_temp_dir("retry-status");
        let file = dir.join("ses-missing.jsonl");
        let mut tailer = JsonlTailer::with_follow_newest(&file, false).unwrap();
        let (tx, _rx) = mpsc::channel(8);
        let (status_tx, mut status_rx) = mpsc::unbounded_channel::<JsonlTailStatus>();

        tailer.sync_once(&tx, Some(&status_tx)).await.unwrap();
        let first = status_rx.recv().await.unwrap();
        assert!(matches!(
            first.state,
            JsonlTailConnectionState::Retrying {
                reason: JsonlTailRetryReason::SourceMissing,
                attempt: 1,
                ..
            }
        ));

        tailer.next_retry_at = Instant::now();
        tailer.sync_once(&tx, Some(&status_tx)).await.unwrap();
        let second = status_rx.recv().await.unwrap();
        assert!(matches!(
            second.state,
            JsonlTailConnectionState::Retrying {
                reason: JsonlTailRetryReason::SourceMissing,
                attempt: 2,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn reports_sleep_wake_recovery_and_switches_to_polling() {
        let dir = unique_temp_dir("sleep-wake");
        let file = dir.join("ses-1.jsonl");
        fs::write(&file, "").unwrap();
        let mut tailer = JsonlTailer::with_follow_newest(&file, false).unwrap();
        let (status_tx, mut status_rx) = mpsc::unbounded_channel::<JsonlTailStatus>();

        tailer.last_loop_tick_at =
            Instant::now() - JSONL_TAIL_SLEEP_WAKE_THRESHOLD - Duration::from_secs(1);
        tailer.observe_tick_gap(Instant::now(), Some(&status_tx));

        let status = status_rx.recv().await.unwrap();
        assert!(matches!(
            status.state,
            JsonlTailConnectionState::SleepWakeRecovery { observed_gap }
                if observed_gap >= JSONL_TAIL_SLEEP_WAKE_THRESHOLD
        ));
        assert_eq!(tailer.mode(), JsonlTailMode::Polling);
        assert_eq!(
            tailer.polling_reason,
            Some(JsonlTailPollingReason::SleepWakeRecovery)
        );
    }

    fn append(path: &PathBuf, content: &str) {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        file.write_all(content.as_bytes()).unwrap();
        file.sync_all().unwrap();
    }

    fn session_started_line(session_id: &str) -> String {
        format!(
            "{{\"type\":\"session_started\",\"timestamp\":\"2026-03-24T12:00:00Z\",\"session_id\":\"{session_id}\"}}\n"
        )
    }

    fn assert_session_start(event: AgentEvent, expected_session_id: &str) {
        match event {
            AgentEvent::SessionStart { session_id, .. } => {
                assert_eq!(session_id, expected_session_id)
            }
            other => panic!("unexpected event while waiting for session_start: {other:?}"),
        }
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("saw-jsonl-tail-{label}-{unique}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}
