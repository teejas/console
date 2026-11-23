//! Orchestrator turns and their records.

use chrono::{DateTime, Utc};
use console_proto::domain::{DelegationDecision, TurnOverride};
use console_proto::event::EventBody;
use console_proto::{SessionId, TurnId};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteExecutor};

use crate::{EventKeys, Result, Store, StoreError, WriteTx, dec, enc, id, now, parse_ts, ts};

#[derive(Debug, Clone, PartialEq)]
pub struct TurnRecord {
    pub id: TurnId,
    pub session_id: SessionId,
    pub idempotency_key: String,
    pub user_message: String,
    /// `pending`, `running`, `done`, or `failed`.
    pub status: String,
    pub decision: Option<DelegationDecision>,
    pub reason: Option<String>,
    pub tool_calls: u32,
    pub wall_ms: Option<u64>,
    pub budget_hit: bool,
    pub override_mode: Option<TurnOverride>,
    pub cost_usd: Option<f64>,
    pub created_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
}

/// What the service records when a turn finishes.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnEnd {
    pub ok: bool,
    pub decision: DelegationDecision,
    pub reason: Option<String>,
    pub wall_ms: u64,
    pub budget_hit: bool,
    pub cost_usd: Option<f64>,
}

fn row_to_turn(row: &SqliteRow) -> Result<TurnRecord> {
    let tool_calls: i64 = row.try_get("tool_calls")?;
    let wall_ms: Option<i64> = row.try_get("wall_ms")?;
    let budget_hit: i64 = row.try_get("budget_hit")?;
    Ok(TurnRecord {
        id: id(&row.try_get::<String, _>("id")?)?,
        session_id: id(&row.try_get::<String, _>("session_id")?)?,
        idempotency_key: row.try_get("idempotency_key")?,
        user_message: row.try_get("user_message")?,
        status: row.try_get("status")?,
        decision: row
            .try_get::<Option<String>, _>("decision")?
            .as_deref()
            .map(dec)
            .transpose()?,
        reason: row.try_get("reason")?,
        tool_calls: u32::try_from(tool_calls).unwrap_or_default(),
        wall_ms: wall_ms.and_then(|w| u64::try_from(w).ok()),
        budget_hit: budget_hit != 0,
        override_mode: row
            .try_get::<Option<String>, _>("override")?
            .as_deref()
            .map(dec)
            .transpose()?,
        cost_usd: row.try_get("cost_usd")?,
        created_at: parse_ts(&row.try_get::<String, _>("created_at")?)?,
        ended_at: row
            .try_get::<Option<String>, _>("ended_at")?
            .as_deref()
            .map(parse_ts)
            .transpose()?,
    })
}

async fn get_turn<'e>(e: impl SqliteExecutor<'e>, tid: TurnId) -> Result<Option<TurnRecord>> {
    sqlx::query("SELECT * FROM turns WHERE id = ?1")
        .bind(tid.to_string())
        .fetch_optional(e)
        .await?
        .as_ref()
        .map(row_to_turn)
        .transpose()
}

impl WriteTx {
    /// Records a user message as a pending turn and appends `UserMessage`. A repeated
    /// idempotency key returns the existing turn and `false`.
    pub async fn insert_turn(
        &mut self,
        session_id: SessionId,
        idempotency_key: &str,
        text: &str,
    ) -> Result<(TurnRecord, bool)> {
        if let Some(row) = sqlx::query("SELECT * FROM turns WHERE idempotency_key = ?1")
            .bind(idempotency_key)
            .fetch_optional(self.conn())
            .await?
        {
            return Ok((row_to_turn(&row)?, false));
        }
        let tid = TurnId::generate();
        sqlx::query(
            "INSERT INTO turns (id, session_id, idempotency_key, user_message, status, created_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5)",
        )
        .bind(tid.to_string())
        .bind(session_id.to_string())
        .bind(idempotency_key)
        .bind(text)
        .bind(ts(now()))
        .execute(self.conn())
        .await?;
        self.append(
            EventKeys::session(session_id),
            EventBody::UserMessage {
                turn_id: tid,
                text: text.to_owned(),
            },
        )
        .await?;
        let turn = get_turn(self.conn(), tid)
            .await?
            .ok_or_else(|| StoreError::NotFound(tid.to_string()))?;
        Ok((turn, true))
    }

    pub async fn set_turn_status(&mut self, tid: TurnId, status: &str) -> Result<()> {
        sqlx::query("UPDATE turns SET status = ?1 WHERE id = ?2")
            .bind(status)
            .bind(tid.to_string())
            .execute(self.conn())
            .await?;
        Ok(())
    }

    /// Counts one inline tool call and returns the new total.
    pub async fn bump_tool_calls(&mut self, tid: TurnId) -> Result<u32> {
        let row = sqlx::query(
            "UPDATE turns SET tool_calls = tool_calls + 1 WHERE id = ?1 RETURNING tool_calls",
        )
        .bind(tid.to_string())
        .fetch_optional(self.conn())
        .await?
        .ok_or_else(|| StoreError::NotFound(tid.to_string()))?;
        let n: i64 = row.try_get("tool_calls")?;
        Ok(u32::try_from(n).unwrap_or_default())
    }

    pub async fn mark_budget_hit(&mut self, tid: TurnId) -> Result<()> {
        sqlx::query("UPDATE turns SET budget_hit = 1 WHERE id = ?1")
            .bind(tid.to_string())
            .execute(self.conn())
            .await?;
        Ok(())
    }

    pub async fn set_turn_override(&mut self, tid: TurnId, mode: TurnOverride) -> Result<()> {
        sqlx::query("UPDATE turns SET override = ?1 WHERE id = ?2")
            .bind(enc(&mode))
            .bind(tid.to_string())
            .execute(self.conn())
            .await?;
        Ok(())
    }

    /// Finishes a turn and appends `TurnEnded`.
    pub async fn end_turn(&mut self, tid: TurnId, end: TurnEnd) -> Result<TurnRecord> {
        sqlx::query(
            "UPDATE turns SET status = ?1, decision = ?2, reason = ?3, wall_ms = ?4,
                              budget_hit = MAX(budget_hit, ?5), cost_usd = ?6, ended_at = ?7
             WHERE id = ?8",
        )
        .bind(if end.ok { "done" } else { "failed" })
        .bind(enc(&end.decision))
        .bind(&end.reason)
        .bind(i64::try_from(end.wall_ms).unwrap_or(i64::MAX))
        .bind(i64::from(end.budget_hit))
        .bind(end.cost_usd)
        .bind(ts(now()))
        .bind(tid.to_string())
        .execute(self.conn())
        .await?;
        let turn = get_turn(self.conn(), tid)
            .await?
            .ok_or_else(|| StoreError::NotFound(tid.to_string()))?;
        self.append(
            EventKeys::session(turn.session_id),
            EventBody::TurnEnded {
                turn_id: tid,
                decision: end.decision,
                tool_calls: turn.tool_calls,
                wall_ms: end.wall_ms,
                budget_hit: turn.budget_hit,
            },
        )
        .await?;
        Ok(turn)
    }
}

impl Store {
    pub async fn turn(&self, tid: TurnId) -> Result<Option<TurnRecord>> {
        get_turn(self.pool(), tid).await
    }

    /// Turns not yet finished, oldest first (for recovery after a restart).
    pub async fn unfinished_turns(&self) -> Result<Vec<TurnRecord>> {
        sqlx::query(
            "SELECT * FROM turns WHERE status IN ('pending', 'running') ORDER BY created_at",
        )
        .fetch_all(self.pool())
        .await?
        .iter()
        .map(row_to_turn)
        .collect()
    }

    /// The most recent turns, newest first.
    pub async fn recent_turns(&self, limit: u32) -> Result<Vec<TurnRecord>> {
        sqlx::query("SELECT * FROM turns ORDER BY created_at DESC LIMIT ?1")
            .bind(limit)
            .fetch_all(self.pool())
            .await?
            .iter()
            .map(row_to_turn)
            .collect()
    }
}
