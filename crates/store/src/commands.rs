//! Attempts and the commands dispatched to executors.

use chrono::{DateTime, Utc};
use console_proto::link::{CommandStatus, Op};
use console_proto::{AttemptId, CommandId, ExecutorId, TaskId};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteExecutor};

use crate::{
    Result, Store, StoreError, WriteTx, dec, enc, from_json, id, json, now, opt_id, parse_ts, ts,
};

#[derive(Debug, Clone, PartialEq)]
pub struct CommandRecord {
    pub id: CommandId,
    pub attempt_id: Option<AttemptId>,
    pub task_id: Option<TaskId>,
    pub executor_id: ExecutorId,
    pub op: Op,
    /// `None` until the executor acknowledges it (stored as `pending`).
    pub status: Option<CommandStatus>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl CommandRecord {
    /// Finished from the Orchestrator's point of view.
    #[must_use]
    pub fn is_final(&self) -> bool {
        matches!(
            self.status,
            Some(CommandStatus::Succeeded | CommandStatus::Failed | CommandStatus::Cancelled)
        )
    }
}

fn row_to_command(row: &SqliteRow) -> Result<CommandRecord> {
    let status: String = row.try_get("status")?;
    Ok(CommandRecord {
        id: id(&row.try_get::<String, _>("id")?)?,
        attempt_id: opt_id(row.try_get("attempt_id")?)?,
        task_id: opt_id(row.try_get("task_id")?)?,
        executor_id: id(&row.try_get::<String, _>("executor_id")?)?,
        op: from_json(&row.try_get::<String, _>("op")?)?,
        status: if status == "pending" {
            None
        } else {
            Some(dec(&status)?)
        },
        created_at: parse_ts(&row.try_get::<String, _>("created_at")?)?,
        updated_at: parse_ts(&row.try_get::<String, _>("updated_at")?)?,
    })
}

async fn get_command<'e>(
    e: impl SqliteExecutor<'e>,
    cid: CommandId,
) -> Result<Option<CommandRecord>> {
    sqlx::query("SELECT * FROM commands WHERE id = ?1")
        .bind(cid.to_string())
        .fetch_optional(e)
        .await?
        .as_ref()
        .map(row_to_command)
        .transpose()
}

impl WriteTx {
    /// Starts a new attempt for a task.
    pub async fn insert_attempt(
        &mut self,
        task_id: TaskId,
        executor_id: ExecutorId,
    ) -> Result<AttemptId> {
        let aid = AttemptId::generate();
        sqlx::query(
            "INSERT INTO attempts (id, task_id, executor_id, created_at) VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(aid.to_string())
        .bind(task_id.to_string())
        .bind(executor_id.to_string())
        .bind(ts(now()))
        .execute(self.conn())
        .await?;
        Ok(aid)
    }

    pub async fn end_attempt(&mut self, aid: AttemptId, outcome: &str) -> Result<()> {
        sqlx::query(
            "UPDATE attempts SET ended_at = ?1, outcome = ?2 WHERE id = ?3 AND ended_at IS NULL",
        )
        .bind(ts(now()))
        .bind(outcome)
        .bind(aid.to_string())
        .execute(self.conn())
        .await?;
        Ok(())
    }

    /// Records a command before it is sent (dispatch intent).
    pub async fn insert_command(
        &mut self,
        executor_id: ExecutorId,
        task_id: Option<TaskId>,
        attempt_id: Option<AttemptId>,
        op: &Op,
    ) -> Result<CommandRecord> {
        let cid = CommandId::generate();
        let at = ts(now());
        sqlx::query(
            "INSERT INTO commands (id, attempt_id, task_id, executor_id, op, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?6)",
        )
        .bind(cid.to_string())
        .bind(attempt_id.map(|a| a.to_string()))
        .bind(task_id.map(|t| t.to_string()))
        .bind(executor_id.to_string())
        .bind(json(op))
        .bind(&at)
        .execute(self.conn())
        .await?;
        get_command(self.conn(), cid)
            .await?
            .ok_or_else(|| StoreError::NotFound(cid.to_string()))
    }

    pub async fn set_command_status(
        &mut self,
        cid: CommandId,
        status: CommandStatus,
    ) -> Result<()> {
        sqlx::query("UPDATE commands SET status = ?1, updated_at = ?2 WHERE id = ?3")
            .bind(enc(&status))
            .bind(ts(now()))
            .bind(cid.to_string())
            .execute(self.conn())
            .await?;
        Ok(())
    }
}

impl Store {
    pub async fn command(&self, cid: CommandId) -> Result<Option<CommandRecord>> {
        get_command(self.pool(), cid).await
    }

    /// Commands on an executor that the Orchestrator doesn't consider finished.
    pub async fn open_commands_for(&self, executor_id: ExecutorId) -> Result<Vec<CommandRecord>> {
        let rows = sqlx::query("SELECT * FROM commands WHERE executor_id = ?1 ORDER BY created_at")
            .bind(executor_id.to_string())
            .fetch_all(self.pool())
            .await?;
        let mut cmds = rows
            .iter()
            .map(row_to_command)
            .collect::<Result<Vec<_>>>()?;
        cmds.retain(|c| !c.is_final());
        Ok(cmds)
    }

    /// The newest command of a task (its current attempt's worker).
    pub async fn latest_command_for_task(&self, task_id: TaskId) -> Result<Option<CommandRecord>> {
        sqlx::query("SELECT * FROM commands WHERE task_id = ?1 ORDER BY created_at DESC LIMIT 1")
            .bind(task_id.to_string())
            .fetch_optional(self.pool())
            .await?
            .as_ref()
            .map(row_to_command)
            .transpose()
    }
}
