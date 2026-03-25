use crate::EventBus;
use chrono::Utc;
use notify::{
    event::{CreateKind, RemoveKind},
    Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use saw_core::{AgentEvent, FileChangeKind, FileModification};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

pub const WATCHER_POLL_INTERVAL: Duration = Duration::from_millis(100);

const NOISE_DIRS: [&str; 6] = [
    ".git",
    "target",
    "node_modules",
    "__pycache__",
    ".saw",
    ".claude",
];

pub struct FileWatcher {
    project_dir: PathBuf,
    guard_paths: Vec<PathBuf>,
    watcher: RecommendedWatcher,
}

impl FileWatcher {
    pub fn new(
        project_dir: impl AsRef<Path>,
        guard_paths: Vec<PathBuf>,
        event_bus: EventBus,
    ) -> notify::Result<Self> {
        let project_dir = normalize_root(project_dir.as_ref());
        let guard_paths = guard_paths
            .into_iter()
            .map(|path| normalize_input_path(&project_dir, path))
            .collect::<Vec<_>>();
        let callback_project_dir = project_dir.clone();
        let callback_guard_paths = guard_paths.clone();

        let mut watcher = RecommendedWatcher::new(
            move |result| {
                handle_notify_result(
                    result,
                    &callback_project_dir,
                    &callback_guard_paths,
                    &event_bus,
                );
            },
            Config::default().with_poll_interval(WATCHER_POLL_INTERVAL),
        )?;
        watcher.watch(&project_dir, RecursiveMode::Recursive)?;

        Ok(Self {
            project_dir,
            guard_paths,
            watcher,
        })
    }

    pub fn project_dir(&self) -> &Path {
        &self.project_dir
    }

    pub fn guard_paths(&self) -> &[PathBuf] {
        &self.guard_paths
    }

    pub fn watcher(&self) -> &RecommendedWatcher {
        &self.watcher
    }
}

fn handle_notify_result(
    result: notify::Result<Event>,
    project_dir: &Path,
    guard_paths: &[PathBuf],
    event_bus: &EventBus,
) {
    match result {
        Ok(event) => publish_file_events(event, project_dir, guard_paths, event_bus),
        Err(error) => log::warn!(
            "file watcher error under {}: {error}",
            project_dir.display()
        ),
    }
}

fn publish_file_events(
    event: Event,
    project_dir: &Path,
    guard_paths: &[PathBuf],
    event_bus: &EventBus,
) {
    let Some(kind) = map_event_kind(&event.kind) else {
        return;
    };
    let timestamp = Utc::now();

    for path in event.paths {
        let path = normalize_input_path(project_dir, path);
        if should_ignore_path(&path) || !should_track_path(&path, &event.kind) {
            continue;
        }

        if let Some(guard_path) = matching_guard_path(&path, guard_paths) {
            log::warn!(
                "file watcher observed out-of-scope path {} outside guard {}",
                path.display(),
                guard_path.display()
            );
        }

        event_bus.publish(AgentEvent::FileModified(FileModification {
            timestamp,
            path,
            kind: kind.clone(),
            line_change: None,
        }));
    }
}

fn map_event_kind(kind: &EventKind) -> Option<FileChangeKind> {
    match kind {
        EventKind::Create(create_kind) if !matches!(create_kind, CreateKind::Folder) => {
            Some(FileChangeKind::Created)
        }
        EventKind::Modify(_) => Some(FileChangeKind::Modified),
        EventKind::Remove(remove_kind) if !matches!(remove_kind, RemoveKind::Folder) => {
            Some(FileChangeKind::Deleted)
        }
        _ => None,
    }
}

fn should_track_path(path: &Path, event_kind: &EventKind) -> bool {
    match event_kind {
        EventKind::Create(CreateKind::Folder) | EventKind::Remove(RemoveKind::Folder) => false,
        _ => !path.exists() || path.is_file(),
    }
}

fn should_ignore_path(path: &Path) -> bool {
    path.components().any(|component| match component {
        Component::Normal(name) => {
            let name = name.to_string_lossy();
            NOISE_DIRS.iter().any(|noise| *noise == name)
        }
        _ => false,
    })
}

fn normalize_root(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn normalize_input_path(project_dir: &Path, path: impl Into<PathBuf>) -> PathBuf {
    let path = path.into();
    let path = if path.is_absolute() {
        path
    } else {
        project_dir.join(path)
    };

    path.canonicalize().unwrap_or(path)
}

fn matching_guard_path<'a>(path: &Path, guard_paths: &'a [PathBuf]) -> Option<&'a PathBuf> {
    if path_matches_any_guard(path, guard_paths) {
        return None;
    }

    guard_paths.iter().max_by_key(|guard_path| {
        (
            shared_prefix_len(path, guard_path),
            guard_path.components().count(),
        )
    })
}

fn path_matches_any_guard(path: &Path, guard_paths: &[PathBuf]) -> bool {
    guard_paths
        .iter()
        .any(|guard_path| path.starts_with(guard_path))
}

fn shared_prefix_len(path: &Path, guard_path: &Path) -> usize {
    path.components()
        .zip(guard_path.components())
        .take_while(|(path_component, guard_component)| path_component == guard_component)
        .count()
}

#[cfg(test)]
mod tests {
    use super::{
        handle_notify_result, matching_guard_path, should_ignore_path, FileWatcher,
        WATCHER_POLL_INTERVAL,
    };
    use crate::{EventBus, Receiver};
    use notify::Error;
    use saw_core::{
        classify, AgentEvent, AgentPhase, AgentState, ClassifierConfig, FileChangeKind,
        FileModification,
    };
    use std::fs::{self, File, OpenOptions};
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use tokio::sync::broadcast::error::TryRecvError;
    use tokio::time::{sleep, timeout};

    #[tokio::test]
    async fn watches_recursively_and_emits_create_modify_remove_events() {
        let project_dir = unique_temp_dir("watch-recursive");
        let nested_dir = project_dir.join("src/nested");
        fs::create_dir_all(&nested_dir).unwrap();

        let bus = EventBus::new();
        let mut receiver = bus.subscribe();
        let _watcher = FileWatcher::new(&project_dir, Vec::new(), bus).unwrap();
        sleep(WATCHER_POLL_INTERVAL * 2).await;

        let file_path = nested_dir.join("example.rs");
        drop(File::create(&file_path).unwrap());
        recv_matching_file_event(&mut receiver, |event| {
            event.path == file_path
                && matches!(
                    event.kind,
                    FileChangeKind::Created | FileChangeKind::Modified
                )
        })
        .await;

        let mut file = OpenOptions::new().append(true).open(&file_path).unwrap();
        writeln!(file, "fn main() {{}}").unwrap();
        file.sync_all().unwrap();
        drop(file);
        recv_matching_file_event(&mut receiver, |event| {
            event.path == file_path && event.kind == FileChangeKind::Modified
        })
        .await;

        fs::remove_file(&file_path).unwrap();
        recv_matching_file_event(&mut receiver, |event| {
            event.path == file_path && event.kind == FileChangeKind::Deleted
        })
        .await;
    }

    #[tokio::test]
    async fn ignores_noise_directories() {
        let project_dir = unique_temp_dir("watch-noise");
        let bus = EventBus::new();
        let mut receiver = bus.subscribe();
        let _watcher = FileWatcher::new(&project_dir, Vec::new(), bus).unwrap();
        sleep(WATCHER_POLL_INTERVAL).await;

        let git_dir = project_dir.join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(
            git_dir.join("config"),
            "[core]\nrepositoryformatversion = 0\n",
        )
        .unwrap();

        assert!(timeout(Duration::from_millis(300), receiver.recv())
            .await
            .is_err());
        assert!(should_ignore_path(&project_dir.join("target/debug/app")));
        assert!(should_ignore_path(
            &project_dir.join("node_modules/pkg/index.js")
        ));
        assert!(should_ignore_path(
            &project_dir.join(".claude/projects/session.jsonl")
        ));
    }

    #[tokio::test]
    async fn guard_violations_classify_as_scope_leaking() {
        let project_dir = unique_temp_dir("watch-guard");
        let auth_dir = project_dir.join("src/auth");
        let billing_dir = project_dir.join("src/billing");
        fs::create_dir_all(&auth_dir).unwrap();
        fs::create_dir_all(&billing_dir).unwrap();

        let bus = EventBus::new();
        let mut receiver = bus.subscribe();
        let _watcher = FileWatcher::new(&project_dir, vec![auth_dir.clone()], bus).unwrap();
        sleep(WATCHER_POLL_INTERVAL * 2).await;

        let violating_file = billing_dir.join("mod.rs");
        fs::write(&violating_file, "pub fn charge() {}\n").unwrap();
        let file_event = recv_matching_file_event(&mut receiver, |event| {
            event.path == violating_file
                && matches!(
                    event.kind,
                    FileChangeKind::Created | FileChangeKind::Modified
                )
        })
        .await;

        let mut state = AgentState {
            guard_paths: vec![auth_dir.clone()],
            ..Default::default()
        };
        state.apply(&AgentEvent::FileModified(file_event.clone()));

        let phase = classify(&state, file_event.timestamp, ClassifierConfig::default());
        assert!(matches!(
            phase,
            AgentPhase::ScopeLeaking {
                violating_file: ref path,
                guard_path: ref guard,
            } if path == &violating_file && guard == &auth_dir
        ));
    }

    #[test]
    fn matching_guard_path_prefers_most_specific_guard() {
        let guards = vec![
            PathBuf::from("/repo/tests/auth"),
            PathBuf::from("/repo/src/auth"),
        ];

        let guard = matching_guard_path(Path::new("/repo/src/billing/mod.rs"), &guards).unwrap();
        assert_eq!(guard, &PathBuf::from("/repo/src/auth"));
    }

    #[test]
    fn notify_errors_are_logged_and_do_not_publish_events() {
        let project_dir = unique_temp_dir("watch-errors");
        let bus = EventBus::new();
        let mut receiver = bus.subscribe();

        handle_notify_result(
            Err(Error::generic(
                "permission denied while reading watched directory",
            )),
            &project_dir,
            &[],
            &bus,
        );

        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("saw-daemon-{label}-{unique}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn recv_matching_file_event<F>(
        receiver: &mut Receiver<AgentEvent>,
        predicate: F,
    ) -> FileModification
    where
        F: Fn(&FileModification) -> bool,
    {
        timeout(Duration::from_secs(5), async {
            loop {
                match receiver.recv().await.unwrap() {
                    AgentEvent::FileModified(event) if predicate(&event) => return event,
                    _ => continue,
                }
            }
        })
        .await
        .unwrap()
    }
}
