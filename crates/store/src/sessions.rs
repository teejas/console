//! Sessions: the Orchestrator conversation and one per worker.

use chrono::{DateTime, Utc};
use console_proto::domain::SessionKind;
use console_proto::{SessionId, TaskId};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteExecutor};

use crate::{Result, Store, StoreError, WriteTx, dec, enc, id, now, opt_id, parse_ts, ts};

#[derive(Debug, Clone, PartialEq)]
pub struct SessionRecord {
    pub id: SessionId,
    pub kind: SessionKind,
    pub task_id: Option<TaskId>,
    /// The harness's own session identifier (Claude Code or ACP session id).
    pub harness_session_ref: Option<String>,
    pub title: String,
    pub created_at: DateTime<Utc>,
}

fn row_to_session(row: &SqliteRow) -> Result<SessionRecord> {
    Ok(SessionRecord {
        id: id(&row.try_get::<String, _>("id")?)?,
        kind: dec(&row.try_get::<String, _>("kind")?)?,
        task_id: opt_id(row.try_get("task_id")?)?,
        harness_session_ref: row.try_get("harness_session_ref")?,
        title: row.try_get("title")?,
        created_at: parse_ts(&row.try_get::<String, _>("created_at")?)?,
    })
}

async fn get_session<'e>(
    e: impl SqliteExecutor<'e>,
    sid: SessionId,
) -> Result<Option<SessionRecord>> {
    sqlx::query("SELECT * FROM sessions WHERE id = ?1")
        .bind(sid.to_string())
        .fetch_optional(e)
        .await?
        .as_ref()
        .map(row_to_session)
        .transpose()
}

impl WriteTx {
    pub async fn insert_session(
        &mut self,
        kind: SessionKind,
        task_id: Option<TaskId>,
        title: &str,
    ) -> Result<SessionRecord> {
        let sid = SessionId::generate();
        sqlx::query(
            "INSERT INTO sessions (id, kind, task_id, title, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(sid.to_string())
        .bind(enc(&kind))
        .bind(task_id.map(|t| t.to_string()))
        .bind(title)
        .bind(ts(now()))
        .execute(self.conn())
        .await?;
        get_session(self.conn(), sid)
            .await?
            .ok_or_else(|| StoreError::NotFound(sid.to_string()))
    }

    pub async fn set_harness_session_ref(&mut self, sid: SessionId, reference: &str) -> Result<()> {
        sqlx::query("UPDATE sessions SET harness_session_ref = ?1 WHERE id = ?2")
            .bind(reference)
            .bind(sid.to_string())
            .execute(self.conn())
            .await?;
        Ok(())
    }
}

impl Store {
    pub async fn session(&self, sid: SessionId) -> Result<Option<SessionRecord>> {
        get_session(self.pool(), sid).await
    }

    /// The single Orchestrator conversation, created on first use.
    pub async fn orchestrator_session(&self) -> Result<SessionRecord> {
        let row = sqlx::query(
            "SELECT * FROM sessions WHERE kind = 'orchestrator' ORDER BY created_at LIMIT 1",
        )
        .fetch_optional(self.pool())
        .await?;
        if let Some(row) = row {
            return row_to_session(&row);
        }
        self.write(async |tx| {
            tx.insert_session(SessionKind::Orchestrator, None, "Orchestrator")
                .await
        })
        .await
    }

    pub async fn worker_session_for(&self, task_id: TaskId) -> Result<Option<SessionRecord>> {
        sqlx::query("SELECT * FROM sessions WHERE kind = 'worker' AND task_id = ?1")
            .bind(task_id.to_string())
            .fetch_optional(self.pool())
            .await?
            .as_ref()
            .map(row_to_session)
            .transpose()
    }
}
