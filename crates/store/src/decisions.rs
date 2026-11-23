//! Decisions: worker questions and permission requests waiting on the user.

use chrono::{DateTime, Utc};
use console_proto::api::DecisionView;
use console_proto::event::{DecisionOption, EventBody};
use console_proto::{DecisionId, TaskId};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteExecutor};

use crate::{
    EventKeys, Result, Store, StoreError, WriteTx, dec, enc, from_json, id, json, now, parse_ts, ts,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionOrigin {
    /// The worker called `ask_orchestrator`; the answer is free text.
    Question,
    /// An ACP permission request; the answer is one of the options.
    Permission,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionStatus {
    Open,
    Resolved,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DecisionRecord {
    pub id: DecisionId,
    pub task_id: TaskId,
    pub origin: DecisionOrigin,
    pub question: String,
    pub options: Vec<DecisionOption>,
    pub status: DecisionStatus,
    pub option_id: Option<String>,
    pub answer: Option<String>,
    pub created_at: DateTime<Utc>,
}

impl DecisionRecord {
    #[must_use]
    pub fn view(&self) -> DecisionView {
        DecisionView {
            id: self.id,
            task_id: self.task_id,
            question: self.question.clone(),
            options: self.options.clone(),
        }
    }
}

fn row_to_decision(row: &SqliteRow) -> Result<DecisionRecord> {
    Ok(DecisionRecord {
        id: id(&row.try_get::<String, _>("id")?)?,
        task_id: id(&row.try_get::<String, _>("task_id")?)?,
        origin: dec(&row.try_get::<String, _>("origin")?)?,
        question: row.try_get("question")?,
        options: from_json(&row.try_get::<String, _>("options")?)?,
        status: dec(&row.try_get::<String, _>("status")?)?,
        option_id: row.try_get("option_id")?,
        answer: row.try_get("answer")?,
        created_at: parse_ts(&row.try_get::<String, _>("created_at")?)?,
    })
}

async fn get_decision<'e>(
    e: impl SqliteExecutor<'e>,
    did: DecisionId,
) -> Result<Option<DecisionRecord>> {
    sqlx::query("SELECT * FROM decisions WHERE id = ?1")
        .bind(did.to_string())
        .fetch_optional(e)
        .await?
        .as_ref()
        .map(row_to_decision)
        .transpose()
}

impl WriteTx {
    /// Opens a decision and appends `DecisionRequested`.
    pub async fn insert_decision(
        &mut self,
        did: DecisionId,
        task_id: TaskId,
        origin: DecisionOrigin,
        question: &str,
        options: Vec<DecisionOption>,
    ) -> Result<DecisionRecord> {
        sqlx::query(
            "INSERT INTO decisions (id, task_id, origin, question, options, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'open', ?6)",
        )
        .bind(did.to_string())
        .bind(task_id.to_string())
        .bind(enc(&origin))
        .bind(question)
        .bind(json(&options))
        .bind(ts(now()))
        .execute(self.conn())
        .await?;
        self.append(
            EventKeys::task(task_id),
            EventBody::DecisionRequested {
                decision_id: did,
                question: question.to_owned(),
                options,
            },
        )
        .await?;
        get_decision(self.conn(), did)
            .await?
            .ok_or_else(|| StoreError::NotFound(did.to_string()))
    }

    /// Resolves an open decision (compare-and-set on `open`) and appends `DecisionResolved`.
    pub async fn resolve_decision(
        &mut self,
        did: DecisionId,
        option_id: &str,
        answer: Option<&str>,
    ) -> Result<DecisionRecord> {
        let n = sqlx::query(
            "UPDATE decisions SET status = 'resolved', option_id = ?1, answer = ?2, resolved_at = ?3
             WHERE id = ?4 AND status = 'open'",
        )
        .bind(option_id)
        .bind(answer)
        .bind(ts(now()))
        .bind(did.to_string())
        .execute(self.conn())
        .await?
        .rows_affected();
        let record = get_decision(self.conn(), did)
            .await?
            .ok_or_else(|| StoreError::NotFound(did.to_string()))?;
        if n != 1 {
            return Err(StoreError::Conflict(format!("{did} is not open")));
        }
        self.append(
            EventKeys::task(record.task_id),
            EventBody::DecisionResolved {
                decision_id: did,
                option_id: option_id.to_owned(),
            },
        )
        .await?;
        Ok(record)
    }

    /// Cancels every open decision of a task (when it ends).
    pub async fn cancel_decisions_for(&mut self, task_id: TaskId) -> Result<u64> {
        Ok(sqlx::query(
            "UPDATE decisions SET status = 'cancelled', resolved_at = ?1
             WHERE task_id = ?2 AND status = 'open'",
        )
        .bind(ts(now()))
        .bind(task_id.to_string())
        .execute(self.conn())
        .await?
        .rows_affected())
    }
}

impl Store {
    pub async fn decision(&self, did: DecisionId) -> Result<Option<DecisionRecord>> {
        get_decision(self.pool(), did).await
    }

    pub async fn open_decisions(&self) -> Result<Vec<DecisionRecord>> {
        sqlx::query("SELECT * FROM decisions WHERE status = 'open' ORDER BY created_at")
            .fetch_all(self.pool())
            .await?
            .iter()
            .map(row_to_decision)
            .collect()
    }
}
