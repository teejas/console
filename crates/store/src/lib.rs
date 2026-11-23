//! SQLite store: migrations, repositories, and the event log with cursor replay (M2).
//!
//! See `docs/specs/state-and-durability.md`.
//!
//! - Reads go through [`Store`] directly and can run concurrently.
//! - Writes go through [`Store::write`], which serializes writers, opens `BEGIN IMMEDIATE`,
//!   and hands the closure a [`WriteTx`]. Events appended in the transaction are broadcast
//!   to subscribers only after it commits (the outbox rule).

mod artifacts;
mod commands;
mod decisions;
mod events;
mod sessions;
mod tasks;
mod todos;
mod turns;
mod workspaces;

use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Utc};
use console_proto::event::Event;
use serde::Serialize;
use serde::de::DeserializeOwned;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Sqlite, SqlitePool, Transaction};
use tokio::sync::{Mutex, broadcast};

pub use artifacts::Artifact;
pub use commands::CommandRecord;
pub use decisions::{DecisionOrigin, DecisionRecord, DecisionStatus};
pub use events::EventFilter;
pub use sessions::SessionRecord;
pub use tasks::{NewTask, TaskRecord};
pub use todos::{NewTodo, ProposalRecord, TodoRecord};
pub use turns::{TurnEnd, TurnRecord};
pub use workspaces::{ExecutorRecord, WorkspaceRecord};

/// How many committed events a slow subscriber may lag behind before it must re-read
/// from the store.
const BROADCAST_CAPACITY: usize = 4096;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("{0} not found")]
    NotFound(String),
    /// A compare-and-set write found the row changed since it was read.
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("invalid stored value: {0}")]
    Corrupt(String),
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

#[derive(Debug)]
pub struct Store {
    pool: SqlitePool,
    write_lock: Mutex<()>,
    events_tx: broadcast::Sender<Event>,
}

impl Store {
    /// Opens (creating if needed) the database file and runs migrations.
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| StoreError::Invalid(format!("create {}: {e}", dir.display())))?;
        }
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(10));
        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(opts)
            .await?;
        Self::from_pool(pool).await
    }

    /// An in-memory store for tests. A single connection keeps the database alive.
    pub async fn open_in_memory() -> Result<Self> {
        let opts = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(opts)
            .await?;
        Self::from_pool(pool).await
    }

    async fn from_pool(pool: SqlitePool) -> Result<Self> {
        sqlx::migrate!("./migrations").run(&pool).await?;
        let (events_tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Ok(Self {
            pool,
            write_lock: Mutex::new(()),
            events_tx,
        })
    }

    pub(crate) fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Committed events, in order, as they are written.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events_tx.subscribe()
    }

    /// Runs `f` in one write transaction. On success the transaction commits and the
    /// events it appended are broadcast; on error it rolls back and nothing is published.
    pub async fn write<T>(&self, f: impl AsyncFnOnce(&mut WriteTx) -> Result<T>) -> Result<T> {
        let _guard = self.write_lock.lock().await;
        let tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut wtx = WriteTx {
            tx,
            events: Vec::new(),
        };
        match f(&mut wtx).await {
            Ok(value) => {
                let WriteTx { tx, events } = wtx;
                tx.commit().await?;
                for event in events {
                    // No receivers is fine: nobody is subscribed yet.
                    let _ = self.events_tx.send(event);
                }
                Ok(value)
            }
            Err(e) => {
                let WriteTx { tx, .. } = wtx;
                tx.rollback().await?;
                Err(e)
            }
        }
    }
}

/// An open write transaction. Repository write methods live on this type.
#[derive(Debug)]
pub struct WriteTx {
    tx: Transaction<'static, Sqlite>,
    events: Vec<Event>,
}

impl WriteTx {
    pub(crate) fn conn(&mut self) -> &mut sqlx::SqliteConnection {
        &mut self.tx
    }
}

/// The routing keys of an event; the body carries the payload.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EventKeys {
    pub session_id: Option<console_proto::SessionId>,
    pub task_id: Option<console_proto::TaskId>,
    pub todo_id: Option<console_proto::TodoId>,
}

impl EventKeys {
    pub fn session(id: console_proto::SessionId) -> Self {
        Self {
            session_id: Some(id),
            ..Self::default()
        }
    }
    pub fn task(id: console_proto::TaskId) -> Self {
        Self {
            task_id: Some(id),
            ..Self::default()
        }
    }
    pub fn todo(id: console_proto::TodoId) -> Self {
        Self {
            todo_id: Some(id),
            ..Self::default()
        }
    }
    #[must_use]
    pub fn with_session(mut self, id: console_proto::SessionId) -> Self {
        self.session_id = Some(id);
        self
    }
    #[must_use]
    pub fn with_task(mut self, id: console_proto::TaskId) -> Self {
        self.task_id = Some(id);
        self
    }
    #[must_use]
    pub fn with_todo(mut self, id: console_proto::TodoId) -> Self {
        self.todo_id = Some(id);
        self
    }
}

// ---- value encoding helpers ----

/// The current time at the store's precision (microseconds), so a value read back
/// equals the value written on every platform.
pub(crate) fn now() -> DateTime<Utc> {
    let t = Utc::now();
    DateTime::from_timestamp_micros(t.timestamp_micros()).unwrap_or(t)
}

pub(crate) fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

pub(crate) fn parse_ts(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| StoreError::Corrupt(format!("timestamp {s:?}: {e}")))
}

/// Encodes a unit-like enum as its serde string (for example `TaskState::Running` → `running`).
pub(crate) fn enc<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        Ok(other) => other.to_string(),
        Err(e) => unreachable!("enum encodes: {e}"),
    }
}

pub(crate) fn dec<T: DeserializeOwned>(s: &str) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(s.to_owned()))
        .map_err(|e| StoreError::Corrupt(format!("{s:?}: {e}")))
}

pub(crate) fn id<T: FromStr>(s: &str) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    s.parse()
        .map_err(|e: T::Err| StoreError::Corrupt(format!("id {s:?}: {e}")))
}

pub(crate) fn opt_id<T: FromStr>(s: Option<String>) -> Result<Option<T>>
where
    T::Err: std::fmt::Display,
{
    s.as_deref().map(id).transpose()
}

pub(crate) fn json<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|e| unreachable!("serializable: {e}"))
}

pub(crate) fn from_json<T: DeserializeOwned>(s: &str) -> Result<T> {
    serde_json::from_str(s).map_err(|e| StoreError::Corrupt(format!("json: {e}")))
}
