//! Delegated tasks. State changes are compare-and-set and always append a
//! `TaskTransition` event in the same transaction.

use chrono::{DateTime, Utc};
use console_proto::api::TaskView;
use console_proto::domain::{BlockedReason, TaskState};
use console_proto::event::EventBody;
use console_proto::{ArtifactId, SessionId, TaskId, TodoId, TurnId, WorkspaceId};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteExecutor};

use crate::{
    EventKeys, Result, Store, StoreError, WriteTx, dec, enc, id, now, opt_id, parse_ts, ts,
};

#[derive(Debug, Clone, PartialEq)]
pub struct TaskRecord {
    pub id: TaskId,
    pub todo_id: TodoId,
    pub title: String,
    pub workspace_id: WorkspaceId,
    pub harness: String,
    pub origin_turn_id: Option<TurnId>,
    pub brief_artifact_id: Option<ArtifactId>,
    pub worker_session_id: Option<SessionId>,
    pub state: TaskState,
    pub blocked: Option<BlockedReason>,
    pub version: u64,
    pub summary: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl TaskRecord {
    #[must_use]
    pub fn view(&self) -> TaskView {
        TaskView {
            id: self.id,
            todo_id: self.todo_id,
            title: self.title.clone(),
            state: self.state,
            blocked: self.blocked,
            workspace_id: self.workspace_id,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewTask {
    pub id: TaskId,
    pub todo_id: TodoId,
    pub title: String,
    pub workspace_id: WorkspaceId,
    pub harness: String,
    pub origin_turn_id: Option<TurnId>,
    pub brief_artifact_id: Option<ArtifactId>,
    pub idempotency_key: Option<String>,
}

fn row_to_task(row: &SqliteRow) -> Result<TaskRecord> {
    let version: i64 = row.try_get("version")?;
    Ok(TaskRecord {
        id: id(&row.try_get::<String, _>("id")?)?,
        todo_id: id(&row.try_get::<String, _>("todo_id")?)?,
        title: row.try_get("title")?,
        workspace_id: id(&row.try_get::<String, _>("workspace_id")?)?,
        harness: row.try_get("harness")?,
        origin_turn_id: opt_id(row.try_get("origin_turn_id")?)?,
        brief_artifact_id: opt_id(row.try_get("brief_artifact_id")?)?,
        worker_session_id: opt_id(row.try_get("worker_session_id")?)?,
        state: dec(&row.try_get::<String, _>("state")?)?,
        blocked: row
            .try_get::<Option<String>, _>("blocked_reason")?
            .as_deref()
            .map(dec)
            .transpose()?,
        version: u64::try_from(version).unwrap_or_default(),
        summary: row.try_get("summary")?,
        created_at: parse_ts(&row.try_get::<String, _>("created_at")?)?,
        updated_at: parse_ts(&row.try_get::<String, _>("updated_at")?)?,
    })
}

async fn get_task<'e>(e: impl SqliteExecutor<'e>, task_id: TaskId) -> Result<Option<TaskRecord>> {
    sqlx::query("SELECT * FROM tasks WHERE id = ?1")
        .bind(task_id.to_string())
        .fetch_optional(e)
        .await?
        .as_ref()
        .map(row_to_task)
        .transpose()
}

impl WriteTx {
    /// Creates a task in `Queued`. With an idempotency key that already exists, returns the
    /// existing task instead (and `false`).
    pub async fn insert_task(&mut self, new: NewTask) -> Result<(TaskRecord, bool)> {
        if let Some(key) = &new.idempotency_key
            && let Some(row) = sqlx::query("SELECT * FROM tasks WHERE idempotency_key = ?1")
                .bind(key)
                .fetch_optional(self.conn())
                .await?
        {
            return Ok((row_to_task(&row)?, false));
        }
        let at = ts(now());
        sqlx::query(
            "INSERT INTO tasks (id, todo_id, title, workspace_id, harness, origin_turn_id,
                                brief_artifact_id, state, version, idempotency_key, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, ?9, ?10, ?10)",
        )
        .bind(new.id.to_string())
        .bind(new.todo_id.to_string())
        .bind(&new.title)
        .bind(new.workspace_id.to_string())
        .bind(&new.harness)
        .bind(new.origin_turn_id.map(|t| t.to_string()))
        .bind(new.brief_artifact_id.map(|a| a.to_string()))
        .bind(enc(&TaskState::Queued))
        .bind(&new.idempotency_key)
        .bind(&at)
        .execute(self.conn())
        .await?;
        let task = self.task(new.id).await?;
        Ok((task, true))
    }

    /// Reads a task inside this transaction.
    pub async fn task(&mut self, task_id: TaskId) -> Result<TaskRecord> {
        get_task(self.conn(), task_id)
            .await?
            .ok_or_else(|| StoreError::NotFound(task_id.to_string()))
    }

    /// Compare-and-set state change. Fails with `Conflict` (and changes nothing) unless the
    /// task is still in `expected` at `expected_version`. Appends a `TaskTransition` event.
    pub async fn transition_task(
        &mut self,
        task_id: TaskId,
        expected: TaskState,
        expected_version: u64,
        to: TaskState,
        blocked: Option<BlockedReason>,
    ) -> Result<TaskRecord> {
        let n = sqlx::query(
            "UPDATE tasks SET state = ?1, blocked_reason = ?2, version = version + 1, updated_at = ?3
             WHERE id = ?4 AND state = ?5 AND version = ?6",
        )
        .bind(enc(&to))
        .bind(blocked.as_ref().map(enc))
        .bind(ts(now()))
        .bind(task_id.to_string())
        .bind(enc(&expected))
        .bind(i64::try_from(expected_version).unwrap_or(i64::MAX))
        .execute(self.conn())
        .await?
        .rows_affected();
        if n != 1 {
            return Err(StoreError::Conflict(format!(
                "{task_id} is no longer {} at version {expected_version}",
                enc(&expected)
            )));
        }
        let task = self.task(task_id).await?;
        self.append(
            EventKeys::task(task_id).with_todo(task.todo_id),
            EventBody::TaskTransition {
                from: expected,
                to,
                blocked,
            },
        )
        .await?;
        Ok(task)
    }

    pub async fn set_task_worker_session(
        &mut self,
        task_id: TaskId,
        session: SessionId,
    ) -> Result<()> {
        sqlx::query("UPDATE tasks SET worker_session_id = ?1, updated_at = ?2 WHERE id = ?3")
            .bind(session.to_string())
            .bind(ts(now()))
            .bind(task_id.to_string())
            .execute(self.conn())
            .await?;
        Ok(())
    }

    pub async fn set_task_summary(&mut self, task_id: TaskId, summary: &str) -> Result<()> {
        sqlx::query("UPDATE tasks SET summary = ?1, updated_at = ?2 WHERE id = ?3")
            .bind(summary)
            .bind(ts(now()))
            .bind(task_id.to_string())
            .execute(self.conn())
            .await?;
        Ok(())
    }
}

impl Store {
    pub async fn task(&self, task_id: TaskId) -> Result<Option<TaskRecord>> {
        get_task(self.pool(), task_id).await
    }

    /// Tasks newest first. Terminal tasks only when `include_done`.
    pub async fn tasks(&self, include_done: bool) -> Result<Vec<TaskRecord>> {
        let rows = sqlx::query("SELECT * FROM tasks ORDER BY created_at DESC")
            .fetch_all(self.pool())
            .await?;
        let mut tasks = rows.iter().map(row_to_task).collect::<Result<Vec<_>>>()?;
        if !include_done {
            tasks.retain(|t| !t.state.is_terminal());
        }
        Ok(tasks)
    }

    /// Queued tasks with no live command: held back by concurrency limits, or a retry
    /// waiting for a slot. Oldest first.
    pub async fn undispatched_tasks(&self) -> Result<Vec<TaskRecord>> {
        sqlx::query(
            "SELECT * FROM tasks t WHERE t.state = 'queued'
               AND NOT EXISTS (SELECT 1 FROM commands c WHERE c.task_id = t.id
                               AND c.status IN ('pending', 'accepted', 'running'))
             ORDER BY t.created_at",
        )
        .fetch_all(self.pool())
        .await?
        .iter()
        .map(row_to_task)
        .collect()
    }

    /// Non-terminal tasks targeting a workspace.
    pub async fn active_tasks_in(&self, workspace_id: WorkspaceId) -> Result<Vec<TaskRecord>> {
        let rows = sqlx::query("SELECT * FROM tasks WHERE workspace_id = ?1")
            .bind(workspace_id.to_string())
            .fetch_all(self.pool())
            .await?;
        let mut tasks = rows.iter().map(row_to_task).collect::<Result<Vec<_>>>()?;
        tasks.retain(|t| !t.state.is_terminal());
        Ok(tasks)
    }
}
