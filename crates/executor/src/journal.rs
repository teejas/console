//! The executor's durable command journal (`journal.db`).
//!
//! A command is written here before it runs, so a repeated dispatch is answered with
//! `Duplicate`, and a command that was running when the process died is reported as
//! `Interrupted` after restart rather than silently re-run.

use std::path::Path;
use std::str::FromStr;

use chrono::Utc;
use console_proto::link::{CommandOutcome, CommandStatus, JournalEntry, Op};
use console_proto::{AttemptId, AuthorityEpoch, CommandId, ExecutorId, WorkspaceId};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};

pub type Result<T> = std::result::Result<T, sqlx::Error>;

#[derive(Debug, Clone)]
pub struct Journal {
    pool: SqlitePool,
}

fn enc<T: serde::Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::String(s)) => s,
        Ok(other) => other.to_string(),
        Err(_) => String::new(),
    }
}

fn dec<T: serde::de::DeserializeOwned>(s: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(s.to_owned())).ok()
}

/// A journaled command that finished but whose result may not have reached the Orchestrator.
#[derive(Debug, Clone)]
pub struct Undelivered {
    pub command_id: CommandId,
    pub outcome: CommandOutcome,
}

impl Journal {
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full);
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await?;
        for stmt in [
            "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS workspaces (path TEXT PRIMARY KEY, id TEXT NOT NULL)",
            "CREATE TABLE IF NOT EXISTS commands (
                id TEXT PRIMARY KEY, attempt_id TEXT NOT NULL, op TEXT NOT NULL,
                status TEXT NOT NULL, outcome TEXT, delivered INTEGER NOT NULL DEFAULT 0,
                updated_at TEXT NOT NULL)",
        ] {
            sqlx::query(stmt).execute(&pool).await?;
        }
        Ok(Self { pool })
    }

    async fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(sqlx::query("SELECT value FROM meta WHERE key = ?1")
            .bind(key)
            .fetch_optional(&self.pool)
            .await?
            .map(|r| r.get::<String, _>("value")))
    }

    async fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query("INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = ?2")
            .bind(key)
            .bind(value)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// This executor's stable identity, created on first start.
    pub async fn executor_id(&self) -> Result<ExecutorId> {
        if let Some(id) = self
            .meta("executor_id")
            .await?
            .and_then(|s| ExecutorId::from_str(&s).ok())
        {
            return Ok(id);
        }
        let id = ExecutorId::generate();
        self.set_meta("executor_id", &id.to_string()).await?;
        Ok(id)
    }

    pub async fn max_epoch_seen(&self) -> Result<AuthorityEpoch> {
        Ok(AuthorityEpoch(
            self.meta("max_epoch")
                .await?
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
        ))
    }

    pub async fn see_epoch(&self, epoch: AuthorityEpoch) -> Result<()> {
        if epoch > self.max_epoch_seen().await? {
            self.set_meta("max_epoch", &epoch.0.to_string()).await?;
        }
        Ok(())
    }

    /// The stable workspace ID for a path, created on first registration.
    pub async fn workspace_id(&self, path: &str) -> Result<WorkspaceId> {
        if let Some(row) = sqlx::query("SELECT id FROM workspaces WHERE path = ?1")
            .bind(path)
            .fetch_optional(&self.pool)
            .await?
            && let Ok(id) = WorkspaceId::from_str(&row.get::<String, _>("id"))
        {
            return Ok(id);
        }
        let id = WorkspaceId::generate();
        sqlx::query("INSERT INTO workspaces (path, id) VALUES (?1, ?2)")
            .bind(path)
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(id)
    }

    /// Records a new command as `Accepted`. Returns the existing status instead if the
    /// command is already journaled (a redelivered dispatch).
    pub async fn accept(
        &self,
        cid: CommandId,
        aid: AttemptId,
        op: &Op,
    ) -> Result<Option<CommandStatus>> {
        if let Some(row) = sqlx::query("SELECT status FROM commands WHERE id = ?1")
            .bind(cid.to_string())
            .fetch_optional(&self.pool)
            .await?
        {
            return Ok(Some(
                dec(&row.get::<String, _>("status")).unwrap_or(CommandStatus::Unknown),
            ));
        }
        sqlx::query(
            "INSERT INTO commands (id, attempt_id, op, status, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(cid.to_string())
        .bind(aid.to_string())
        .bind(serde_json::to_string(op).unwrap_or_default())
        .bind(enc(&CommandStatus::Accepted))
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(None)
    }

    pub async fn set_status(&self, cid: CommandId, status: CommandStatus) -> Result<()> {
        sqlx::query("UPDATE commands SET status = ?1, updated_at = ?2 WHERE id = ?3")
            .bind(enc(&status))
            .bind(Utc::now().to_rfc3339())
            .bind(cid.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Records a final outcome (not yet delivered).
    pub async fn finish(&self, cid: CommandId, outcome: &CommandOutcome) -> Result<()> {
        let status = match outcome {
            CommandOutcome::Succeeded { .. } => CommandStatus::Succeeded,
            CommandOutcome::Failed { .. } => CommandStatus::Failed,
            CommandOutcome::Cancelled => CommandStatus::Cancelled,
            CommandOutcome::Interrupted => CommandStatus::Interrupted,
        };
        sqlx::query(
            "UPDATE commands SET status = ?1, outcome = ?2, delivered = 0, updated_at = ?3 WHERE id = ?4",
        )
        .bind(enc(&status))
        .bind(serde_json::to_string(outcome).unwrap_or_default())
        .bind(Utc::now().to_rfc3339())
        .bind(cid.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_delivered(&self, cid: CommandId) -> Result<()> {
        sqlx::query("UPDATE commands SET delivered = 1 WHERE id = ?1")
            .bind(cid.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// At startup: anything still accepted or running died with the previous process.
    pub async fn interrupt_unfinished(&self) -> Result<u64> {
        let outcome = serde_json::to_string(&CommandOutcome::Interrupted).unwrap_or_default();
        Ok(sqlx::query(
            "UPDATE commands SET status = ?1, outcome = ?2, delivered = 0, updated_at = ?3
             WHERE status IN ('accepted', 'running')",
        )
        .bind(enc(&CommandStatus::Interrupted))
        .bind(outcome)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?
        .rows_affected())
    }

    pub async fn status(&self, cid: CommandId) -> Result<CommandStatus> {
        Ok(sqlx::query("SELECT status FROM commands WHERE id = ?1")
            .bind(cid.to_string())
            .fetch_optional(&self.pool)
            .await?
            .and_then(|r| dec(&r.get::<String, _>("status")))
            .unwrap_or(CommandStatus::Unknown))
    }

    /// Entries for the `Hello` message: everything not yet delivered, plus anything live.
    pub async fn entries(&self) -> Result<Vec<JournalEntry>> {
        let rows = sqlx::query(
            "SELECT id, attempt_id, status, updated_at FROM commands
             WHERE delivered = 0 OR status IN ('accepted', 'running') ORDER BY updated_at",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                Some(JournalEntry {
                    command_id: CommandId::from_str(&r.get::<String, _>("id")).ok()?,
                    attempt_id: AttemptId::from_str(&r.get::<String, _>("attempt_id")).ok()?,
                    status: dec(&r.get::<String, _>("status"))?,
                    updated_at: chrono::DateTime::parse_from_rfc3339(
                        &r.get::<String, _>("updated_at"),
                    )
                    .ok()?
                    .with_timezone(&Utc),
                })
            })
            .collect())
    }

    /// Finished outcomes that may not have reached the Orchestrator.
    pub async fn undelivered(&self) -> Result<Vec<Undelivered>> {
        let rows = sqlx::query(
            "SELECT id, outcome FROM commands WHERE delivered = 0 AND outcome IS NOT NULL ORDER BY updated_at",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                Some(Undelivered {
                    command_id: CommandId::from_str(&r.get::<String, _>("id")).ok()?,
                    outcome: serde_json::from_str(&r.get::<String, _>("outcome")).ok()?,
                })
            })
            .collect())
    }
}
