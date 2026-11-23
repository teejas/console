//! Artifacts: briefs and patches.

use chrono::{DateTime, Utc};
use console_proto::{ArtifactId, TaskId};
use sqlx::Row;

use crate::{Result, Store, WriteTx, id, now, opt_id, parse_ts, ts};

#[derive(Debug, Clone, PartialEq)]
pub struct Artifact {
    pub id: ArtifactId,
    pub task_id: Option<TaskId>,
    /// `brief` or `patch`.
    pub kind: String,
    pub content: String,
    pub created_at: DateTime<Utc>,
}

impl WriteTx {
    pub async fn insert_artifact(
        &mut self,
        task_id: Option<TaskId>,
        kind: &str,
        content: &str,
    ) -> Result<ArtifactId> {
        let aid = ArtifactId::generate();
        sqlx::query(
            "INSERT INTO artifacts (id, task_id, kind, content, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(aid.to_string())
        .bind(task_id.map(|t| t.to_string()))
        .bind(kind)
        .bind(content)
        .bind(ts(now()))
        .execute(self.conn())
        .await?;
        Ok(aid)
    }

    pub async fn link_artifact(&mut self, aid: ArtifactId, task_id: TaskId) -> Result<()> {
        sqlx::query("UPDATE artifacts SET task_id = ?1 WHERE id = ?2")
            .bind(task_id.to_string())
            .bind(aid.to_string())
            .execute(self.conn())
            .await?;
        Ok(())
    }
}

impl Store {
    pub async fn artifact(&self, aid: ArtifactId) -> Result<Option<Artifact>> {
        let Some(row) = sqlx::query("SELECT * FROM artifacts WHERE id = ?1")
            .bind(aid.to_string())
            .fetch_optional(self.pool())
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(Artifact {
            id: id(&row.try_get::<String, _>("id")?)?,
            task_id: opt_id(row.try_get("task_id")?)?,
            kind: row.try_get("kind")?,
            content: row.try_get("content")?,
            created_at: parse_ts(&row.try_get::<String, _>("created_at")?)?,
        }))
    }

    /// The newest artifact of a kind for a task.
    pub async fn latest_artifact(&self, task_id: TaskId, kind: &str) -> Result<Option<Artifact>> {
        let Some(row) = sqlx::query(
            "SELECT id FROM artifacts WHERE task_id = ?1 AND kind = ?2 ORDER BY created_at DESC LIMIT 1",
        )
        .bind(task_id.to_string())
        .bind(kind)
        .fetch_optional(self.pool())
        .await?
        else {
            return Ok(None);
        };
        self.artifact(id(&row.try_get::<String, _>("id")?)?).await
    }
}
