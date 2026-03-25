use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use saw_core::{AgentPhase, AgentState};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

const DEFAULT_POOL_SIZE: usize = 4;
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub session_id: String,
    pub started_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
    pub phase: AgentPhase,
    pub total_tool_calls: u64,
    pub touched_file_count: usize,
    pub checkpoint_count: u64,
    pub latest_checkpoint_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct Store {
    db_path: PathBuf,
    pool: Arc<ConnectionPool>,
}

impl Store {
    pub fn new() -> Result<Self> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .context("HOME is not set; cannot resolve ~/.saw/saw.db")?;
        Self::open_in_home(home)
    }

    pub fn open_in_home<P: AsRef<Path>>(home: P) -> Result<Self> {
        Self::open(home.as_ref().join(".saw").join("saw.db"))
    }

    pub fn open<P: AsRef<Path>>(db_path: P) -> Result<Self> {
        Self::open_with_pool_size(db_path, DEFAULT_POOL_SIZE)
    }

    fn open_with_pool_size<P: AsRef<Path>>(db_path: P, max_pool_size: usize) -> Result<Self> {
        let db_path = db_path.as_ref().to_path_buf();
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let pool = Arc::new(ConnectionPool::new(&db_path, max_pool_size)?);
        Ok(Self { db_path, pool })
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    pub fn save_session(&self, session_id: &str, state: &AgentState) -> Result<()> {
        let stored_state = canonicalize_state(session_id, state);
        let updated_at_ms = Utc::now().timestamp_millis();
        let started_at_ms = stored_state
            .session_started_at
            .map(|ts| ts.timestamp_millis());
        let last_event_at_ms = stored_state.last_event_at.map(|ts| ts.timestamp_millis());
        let state_json =
            serde_json::to_string(&stored_state).context("failed to serialize AgentState")?;
        let phase_json =
            serde_json::to_string(&stored_state.phase).context("failed to serialize AgentPhase")?;
        let total_tool_calls = i64::try_from(stored_state.total_tool_calls)
            .context("total_tool_calls exceeds SQLite INTEGER range")?;
        let touched_file_count = i64::try_from(stored_state.touched_files.len())
            .context("touched_file_count exceeds SQLite INTEGER range")?;

        let mut conn = self.pool.checkout()?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO sessions (
                session_id,
                state_json,
                phase_json,
                started_at_ms,
                updated_at_ms,
                last_event_at_ms,
                total_tool_calls,
                touched_file_count
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ON CONFLICT(session_id) DO UPDATE SET
                state_json = excluded.state_json,
                phase_json = excluded.phase_json,
                started_at_ms = COALESCE(excluded.started_at_ms, sessions.started_at_ms),
                updated_at_ms = excluded.updated_at_ms,
                last_event_at_ms = COALESCE(excluded.last_event_at_ms, sessions.last_event_at_ms),
                total_tool_calls = excluded.total_tool_calls,
                touched_file_count = excluded.touched_file_count",
            params![
                session_id,
                state_json,
                phase_json,
                started_at_ms,
                updated_at_ms,
                last_event_at_ms,
                total_tool_calls,
                touched_file_count,
            ],
        )?;
        tx.execute(
            "INSERT INTO events (
                session_id,
                recorded_at_ms,
                event_kind,
                phase_json,
                state_json,
                total_tool_calls,
                touched_file_count
            ) VALUES (?1, ?2, 'state_snapshot', ?3, ?4, ?5, ?6)",
            params![
                session_id,
                updated_at_ms,
                &phase_json,
                &state_json,
                total_tool_calls,
                touched_file_count,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn load_session(&self, session_id: &str) -> Result<Option<AgentState>> {
        let conn = self.pool.checkout()?;
        let state_json = conn
            .query_row(
                "SELECT state_json FROM sessions WHERE session_id = ?1",
                params![session_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        state_json
            .map(|json| {
                let mut state: AgentState = serde_json::from_str(&json)
                    .context("failed to deserialize stored AgentState")?;
                state.session_id = Some(session_id.to_string());
                Ok(state)
            })
            .transpose()
    }

    pub fn save_checkpoint<P: AsRef<Path>>(
        &self,
        session_id: &str,
        checkpoint_path: P,
        files: &[PathBuf],
    ) -> Result<()> {
        let created_at_ms = Utc::now().timestamp_millis();
        let checkpoint_path = checkpoint_path.as_ref().to_path_buf();
        let placeholder_state = canonicalize_state(session_id, &AgentState::default());
        let placeholder_state_json = serde_json::to_string(&placeholder_state)
            .context("failed to serialize placeholder AgentState")?;
        let placeholder_phase_json = serde_json::to_string(&placeholder_state.phase)
            .context("failed to serialize placeholder AgentPhase")?;
        let files_json =
            serde_json::to_string(files).context("failed to serialize checkpoint files")?;
        let file_count = i64::try_from(files.len())
            .context("checkpoint file count exceeds SQLite INTEGER range")?;

        let mut conn = self.pool.checkout()?;
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO sessions (
                session_id,
                state_json,
                phase_json,
                started_at_ms,
                updated_at_ms,
                last_event_at_ms,
                total_tool_calls,
                touched_file_count
            ) VALUES (?1, ?2, ?3, NULL, ?4, NULL, 0, 0)
            ON CONFLICT(session_id) DO UPDATE SET
                updated_at_ms = excluded.updated_at_ms",
            params![
                session_id,
                placeholder_state_json,
                placeholder_phase_json,
                created_at_ms,
            ],
        )?;
        tx.execute(
            "INSERT INTO checkpoints (
                session_id,
                checkpoint_path,
                files_json,
                file_count,
                created_at_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                session_id,
                checkpoint_path.to_string_lossy(),
                files_json,
                file_count,
                created_at_ms,
            ],
        )?;

        let (phase_json, state_json, total_tool_calls, touched_file_count) = tx
            .query_row(
                "SELECT phase_json, state_json, total_tool_calls, touched_file_count
                 FROM sessions WHERE session_id = ?1",
                params![session_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .context("failed to reload session metadata after checkpoint save")?;

        tx.execute(
            "INSERT INTO events (
                session_id,
                recorded_at_ms,
                event_kind,
                phase_json,
                state_json,
                total_tool_calls,
                touched_file_count
            ) VALUES (?1, ?2, 'checkpoint_saved', ?3, ?4, ?5, ?6)",
            params![
                session_id,
                created_at_ms,
                phase_json,
                state_json,
                total_tool_calls,
                touched_file_count,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn list_recent_sessions(&self, limit: usize) -> Result<Vec<SessionSummary>> {
        let limit = i64::try_from(limit).context("limit exceeds SQLite INTEGER range")?;
        let conn = self.pool.checkout()?;
        let mut stmt = conn.prepare(
            "SELECT
                s.session_id,
                s.started_at_ms,
                s.updated_at_ms,
                s.phase_json,
                s.total_tool_calls,
                s.touched_file_count,
                COUNT(c.id) AS checkpoint_count,
                MAX(c.created_at_ms) AS latest_checkpoint_at_ms
             FROM sessions s
             LEFT JOIN checkpoints c ON c.session_id = s.session_id
             GROUP BY
                s.session_id,
                s.started_at_ms,
                s.updated_at_ms,
                s.phase_json,
                s.total_tool_calls,
                s.touched_file_count
             ORDER BY s.updated_at_ms DESC
             LIMIT ?1",
        )?;

        let rows = stmt.query_map(params![limit], |row| {
            let updated_at_ms = row.get::<_, i64>(2)?;
            let phase_json = row.get::<_, String>(3)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                updated_at_ms,
                phase_json,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, Option<i64>>(7)?,
            ))
        })?;

        rows.map(|row| {
            let (
                session_id,
                started_at_ms,
                updated_at_ms,
                phase_json,
                total_tool_calls,
                touched_file_count,
                checkpoint_count,
                latest_checkpoint_at_ms,
            ) = row?;

            Ok(SessionSummary {
                session_id,
                started_at: started_at_ms.map(datetime_from_millis).transpose()?,
                updated_at: datetime_from_millis(updated_at_ms)?,
                phase: serde_json::from_str(&phase_json)
                    .context("failed to deserialize AgentPhase from sessions table")?,
                total_tool_calls: u64::try_from(total_tool_calls)
                    .context("stored total_tool_calls is negative")?,
                touched_file_count: usize::try_from(touched_file_count)
                    .context("stored touched_file_count is negative")?,
                checkpoint_count: u64::try_from(checkpoint_count)
                    .context("stored checkpoint_count is negative")?,
                latest_checkpoint_at: latest_checkpoint_at_ms
                    .map(datetime_from_millis)
                    .transpose()?,
            })
        })
        .collect()
    }
}

fn canonicalize_state(session_id: &str, state: &AgentState) -> AgentState {
    let mut stored_state = state.clone();
    stored_state.session_id = Some(session_id.to_string());
    stored_state
}

fn datetime_from_millis(timestamp_ms: i64) -> Result<DateTime<Utc>> {
    Utc.timestamp_millis_opt(timestamp_ms)
        .single()
        .with_context(|| format!("invalid stored timestamp: {timestamp_ms}"))
}

#[derive(Debug)]
struct ConnectionPool {
    path: PathBuf,
    max_size: usize,
    state: Mutex<PoolState>,
    available: Condvar,
}

#[derive(Debug, Default)]
struct PoolState {
    idle: Vec<Connection>,
    total: usize,
}

impl ConnectionPool {
    fn new(path: &Path, max_size: usize) -> Result<Self> {
        let connection = open_connection(path)?;
        initialize_schema(&connection)?;

        Ok(Self {
            path: path.to_path_buf(),
            max_size: max_size.max(1),
            state: Mutex::new(PoolState {
                idle: vec![connection],
                total: 1,
            }),
            available: Condvar::new(),
        })
    }

    fn checkout(&self) -> Result<PooledConnection<'_>> {
        let mut state = self
            .state
            .lock()
            .expect("SQLite connection pool mutex poisoned");

        loop {
            if let Some(connection) = state.idle.pop() {
                return Ok(PooledConnection {
                    pool: self,
                    connection: Some(connection),
                });
            }

            if state.total < self.max_size {
                state.total += 1;
                drop(state);
                match open_connection(&self.path) {
                    Ok(connection) => {
                        return Ok(PooledConnection {
                            pool: self,
                            connection: Some(connection),
                        });
                    }
                    Err(error) => {
                        let mut state = self
                            .state
                            .lock()
                            .expect("SQLite connection pool mutex poisoned");
                        state.total -= 1;
                        self.available.notify_one();
                        return Err(error);
                    }
                }
            }

            state = self
                .available
                .wait(state)
                .expect("SQLite connection pool mutex poisoned");
        }
    }

    fn release(&self, connection: Connection) {
        let mut state = self
            .state
            .lock()
            .expect("SQLite connection pool mutex poisoned");
        state.idle.push(connection);
        drop(state);
        self.available.notify_one();
    }
}

struct PooledConnection<'a> {
    pool: &'a ConnectionPool,
    connection: Option<Connection>,
}

impl Deref for PooledConnection<'_> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.connection
            .as_ref()
            .expect("pooled SQLite connection missing")
    }
}

impl DerefMut for PooledConnection<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.connection
            .as_mut()
            .expect("pooled SQLite connection missing")
    }
}

impl Drop for PooledConnection<'_> {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            self.pool.release(connection);
        }
    }
}

fn open_connection(path: &Path) -> Result<Connection> {
    let connection = Connection::open(path)
        .with_context(|| format!("failed to open SQLite database at {}", path.display()))?;
    connection.busy_timeout(SQLITE_BUSY_TIMEOUT)?;
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;",
    )?;
    Ok(connection)
}

fn initialize_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS sessions (
            session_id TEXT PRIMARY KEY,
            state_json TEXT NOT NULL,
            phase_json TEXT NOT NULL,
            started_at_ms INTEGER,
            updated_at_ms INTEGER NOT NULL,
            last_event_at_ms INTEGER,
            total_tool_calls INTEGER NOT NULL,
            touched_file_count INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            recorded_at_ms INTEGER NOT NULL,
            event_kind TEXT NOT NULL,
            phase_json TEXT NOT NULL,
            state_json TEXT NOT NULL,
            total_tool_calls INTEGER NOT NULL,
            touched_file_count INTEGER NOT NULL,
            FOREIGN KEY(session_id) REFERENCES sessions(session_id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_events_session_recorded_at
            ON events(session_id, recorded_at_ms DESC);
        CREATE TABLE IF NOT EXISTS checkpoints (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            checkpoint_path TEXT NOT NULL,
            files_json TEXT NOT NULL,
            file_count INTEGER NOT NULL,
            created_at_ms INTEGER NOT NULL,
            FOREIGN KEY(session_id) REFERENCES sessions(session_id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_checkpoints_session_created_at
            ON checkpoints(session_id, created_at_ms DESC);
        CREATE INDEX IF NOT EXISTS idx_sessions_updated_at
            ON sessions(updated_at_ms DESC);",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Store;
    use chrono::{TimeZone, Utc};
    use rusqlite::{params, Connection};
    use saw_core::{AgentPhase, AgentState, FileChangeKind, FileModification};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn creates_database_on_first_run() {
        let home = unique_temp_dir("store-home");
        let db_path = home.join(".saw").join("saw.db");

        let store = Store::open_in_home(&home).unwrap();

        assert_eq!(store.db_path(), db_path.as_path());
        assert!(db_path.exists(), "expected {} to exist", db_path.display());
    }

    #[test]
    fn saves_and_loads_session_state() {
        let store = test_store("save-load");
        let mut state = AgentState {
            session_id: Some("ignored-session-id".into()),
            session_started_at: Some(ts(0)),
            last_event_at: Some(ts(5)),
            total_tool_calls: 3,
            phase: AgentPhase::Working,
            ..Default::default()
        };
        state.touched_files.insert(PathBuf::from("src/lib.rs"));
        state.recently_modified_files.push_back(FileModification {
            timestamp: ts(5),
            path: PathBuf::from("src/lib.rs"),
            kind: FileChangeKind::Modified,
            line_change: None,
        });

        store.save_session("ses-123", &state).unwrap();
        let loaded = store.load_session("ses-123").unwrap().unwrap();

        assert_eq!(loaded.session_id.as_deref(), Some("ses-123"));
        assert_eq!(loaded.session_started_at, Some(ts(0)));
        assert_eq!(loaded.last_event_at, Some(ts(5)));
        assert_eq!(loaded.total_tool_calls, 3);
        assert_eq!(loaded.phase, AgentPhase::Working);
        assert!(loaded.touched_files.contains(&PathBuf::from("src/lib.rs")));
        assert_eq!(loaded.recently_modified_files.len(), 1);

        let conn = Connection::open(store.db_path()).unwrap();
        let event_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE session_id = ?1 AND event_kind = 'state_snapshot'",
                params!["ses-123"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(event_count, 1);
    }

    #[test]
    fn saves_checkpoint_metadata_and_lists_recent_sessions() {
        let store = test_store("checkpoint");
        let state = AgentState {
            session_started_at: Some(ts(0)),
            phase: AgentPhase::Thinking,
            total_tool_calls: 7,
            ..Default::default()
        };
        store.save_session("ses-checkpoint", &state).unwrap();

        let checkpoint_path = PathBuf::from("/tmp/checkpoints/20260325-000000");
        let files = vec![PathBuf::from("src/main.rs"), PathBuf::from("README.md")];
        store
            .save_checkpoint("ses-checkpoint", &checkpoint_path, &files)
            .unwrap();

        let summaries = store.list_recent_sessions(5).unwrap();
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].session_id, "ses-checkpoint");
        assert_eq!(summaries[0].phase, AgentPhase::Thinking);
        assert_eq!(summaries[0].total_tool_calls, 7);
        assert_eq!(summaries[0].checkpoint_count, 1);
        assert!(summaries[0].latest_checkpoint_at.is_some());

        let conn = Connection::open(store.db_path()).unwrap();
        let (path, file_count, files_json): (String, i64, String) = conn
            .query_row(
                "SELECT checkpoint_path, file_count, files_json FROM checkpoints WHERE session_id = ?1",
                params!["ses-checkpoint"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(path, checkpoint_path.to_string_lossy());
        assert_eq!(file_count, 2);
        let stored_files: Vec<PathBuf> = serde_json::from_str(&files_json).unwrap();
        assert_eq!(stored_files, files);

        let checkpoint_events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE session_id = ?1 AND event_kind = 'checkpoint_saved'",
                params!["ses-checkpoint"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(checkpoint_events, 1);
    }

    #[test]
    fn supports_concurrent_session_writes() {
        let store = Arc::new(test_store("concurrent"));
        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();

        for index in 0..8 {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                let state = AgentState {
                    session_started_at: Some(ts(index as i64)),
                    last_event_at: Some(ts(index as i64 + 1)),
                    total_tool_calls: (index + 1) as u64,
                    phase: AgentPhase::Working,
                    ..Default::default()
                };
                barrier.wait();
                store
                    .save_session(&format!("ses-{index}"), &state)
                    .expect("save_session should succeed concurrently");
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        let summaries = store.list_recent_sessions(16).unwrap();
        assert_eq!(summaries.len(), 8);
        for index in 0..8 {
            let session_id = format!("ses-{index}");
            let loaded = store.load_session(&session_id).unwrap();
            assert!(loaded.is_some(), "missing {session_id}");
        }
    }

    fn test_store(label: &str) -> Store {
        let dir = unique_temp_dir(label);
        Store::open(dir.join("saw.db")).unwrap()
    }

    fn ts(seconds_after_start: i64) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 3, 25, 0, 0, 0).single().unwrap()
            + chrono::Duration::seconds(seconds_after_start)
    }

    fn unique_temp_dir(prefix: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("saw-store-{prefix}-{unique}"));
        fs::create_dir_all(&path).unwrap();
        path
    }
}
