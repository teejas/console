//! Todos and proposals. Persistence only: ranking and ownership rules live in the
//! `queue` crate, and the caller appends `TodoChanged` events in the same transaction.

use chrono::{DateTime, Utc};
use console_proto::domain::{Score, TodoKind, TodoOwner, TodoStatus};
use console_proto::{ProposalId, TodoId, WorkspaceId};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteExecutor};

use crate::{
    Result, Store, StoreError, WriteTx, dec, enc, from_json, id, json, now, opt_id, parse_ts, ts,
};

#[derive(Debug, Clone, PartialEq)]
pub struct TodoRecord {
    pub id: TodoId,
    pub title: String,
    pub body: Option<String>,
    pub kind: TodoKind,
    pub owner: TodoOwner,
    pub status: TodoStatus,
    pub importance: Score,
    pub urgency: Score,
    pub due_at: Option<DateTime<Utc>>,
    pub manual_rank: Option<String>,
    pub workspace_id: Option<WorkspaceId>,
    /// Fields the user has set; the Orchestrator may only propose changes to these.
    pub user_fields: Vec<String>,
    pub version: u64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewTodo {
    pub title: String,
    pub body: Option<String>,
    pub kind: TodoKind,
    pub owner: TodoOwner,
    pub status: TodoStatus,
    pub importance: Score,
    pub urgency: Score,
    pub due_at: Option<DateTime<Utc>>,
    pub workspace_id: Option<WorkspaceId>,
    pub user_fields: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProposalRecord {
    pub id: ProposalId,
    pub todo_id: TodoId,
    pub field: String,
    pub value: serde_json::Value,
    pub rationale: String,
    /// `open`, `accepted`, `rejected`, or `expired`.
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

fn score(v: i64) -> Result<Score> {
    u8::try_from(v)
        .ok()
        .and_then(|v| Score::try_from(v).ok())
        .ok_or_else(|| StoreError::Corrupt(format!("score {v}")))
}

fn row_to_todo(row: &SqliteRow) -> Result<TodoRecord> {
    let version: i64 = row.try_get("version")?;
    Ok(TodoRecord {
        id: id(&row.try_get::<String, _>("id")?)?,
        title: row.try_get("title")?,
        body: row.try_get("body")?,
        kind: dec(&row.try_get::<String, _>("kind")?)?,
        owner: dec(&row.try_get::<String, _>("owner")?)?,
        status: dec(&row.try_get::<String, _>("status")?)?,
        importance: score(row.try_get("importance")?)?,
        urgency: score(row.try_get("urgency")?)?,
        due_at: row
            .try_get::<Option<String>, _>("due_at")?
            .as_deref()
            .map(parse_ts)
            .transpose()?,
        manual_rank: row.try_get("manual_rank")?,
        workspace_id: opt_id(row.try_get("workspace_id")?)?,
        user_fields: from_json(&row.try_get::<String, _>("user_fields")?)?,
        version: u64::try_from(version).unwrap_or_default(),
        created_at: parse_ts(&row.try_get::<String, _>("created_at")?)?,
        updated_at: parse_ts(&row.try_get::<String, _>("updated_at")?)?,
    })
}

fn row_to_proposal(row: &SqliteRow) -> Result<ProposalRecord> {
    Ok(ProposalRecord {
        id: id(&row.try_get::<String, _>("id")?)?,
        todo_id: id(&row.try_get::<String, _>("todo_id")?)?,
        field: row.try_get("field")?,
        value: from_json(&row.try_get::<String, _>("value")?)?,
        rationale: row.try_get("rationale")?,
        status: row.try_get("status")?,
        created_at: parse_ts(&row.try_get::<String, _>("created_at")?)?,
        expires_at: parse_ts(&row.try_get::<String, _>("expires_at")?)?,
    })
}

async fn get_todo<'e>(e: impl SqliteExecutor<'e>, tid: TodoId) -> Result<Option<TodoRecord>> {
    sqlx::query("SELECT * FROM todos WHERE id = ?1")
        .bind(tid.to_string())
        .fetch_optional(e)
        .await?
        .as_ref()
        .map(row_to_todo)
        .transpose()
}

impl WriteTx {
    pub async fn insert_todo(&mut self, new: NewTodo) -> Result<TodoRecord> {
        let tid = TodoId::generate();
        let at = ts(now());
        sqlx::query(
            "INSERT INTO todos (id, title, body, kind, owner, status, importance, urgency, due_at,
                                workspace_id, user_fields, version, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 1, ?12, ?12)",
        )
        .bind(tid.to_string())
        .bind(&new.title)
        .bind(&new.body)
        .bind(enc(&new.kind))
        .bind(enc(&new.owner))
        .bind(enc(&new.status))
        .bind(i64::from(new.importance.get()))
        .bind(i64::from(new.urgency.get()))
        .bind(new.due_at.map(ts))
        .bind(new.workspace_id.map(|w| w.to_string()))
        .bind(json(&new.user_fields))
        .bind(&at)
        .execute(self.conn())
        .await?;
        self.todo(tid).await
    }

    pub async fn todo(&mut self, tid: TodoId) -> Result<TodoRecord> {
        get_todo(self.conn(), tid)
            .await?
            .ok_or_else(|| StoreError::NotFound(tid.to_string()))
    }

    /// Compare-and-set update of every mutable field. Fails with `Conflict` unless the
    /// stored version equals `todo.version`; returns the record with the new version.
    pub async fn update_todo(&mut self, todo: &TodoRecord) -> Result<TodoRecord> {
        let n = sqlx::query(
            "UPDATE todos SET title = ?1, body = ?2, kind = ?3, owner = ?4, status = ?5,
                              importance = ?6, urgency = ?7, due_at = ?8, manual_rank = ?9,
                              workspace_id = ?10, user_fields = ?11, version = version + 1,
                              updated_at = ?12
             WHERE id = ?13 AND version = ?14",
        )
        .bind(&todo.title)
        .bind(&todo.body)
        .bind(enc(&todo.kind))
        .bind(enc(&todo.owner))
        .bind(enc(&todo.status))
        .bind(i64::from(todo.importance.get()))
        .bind(i64::from(todo.urgency.get()))
        .bind(todo.due_at.map(ts))
        .bind(&todo.manual_rank)
        .bind(todo.workspace_id.map(|w| w.to_string()))
        .bind(json(&todo.user_fields))
        .bind(ts(now()))
        .bind(todo.id.to_string())
        .bind(i64::try_from(todo.version).unwrap_or(i64::MAX))
        .execute(self.conn())
        .await?
        .rows_affected();
        if n != 1 {
            return Err(StoreError::Conflict(format!(
                "{} changed since version {}",
                todo.id, todo.version
            )));
        }
        self.todo(todo.id).await
    }

    pub async fn insert_proposal(
        &mut self,
        todo_id: TodoId,
        field: &str,
        value: &serde_json::Value,
        rationale: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<ProposalRecord> {
        let pid = ProposalId::generate();
        // A newer proposal for the same field replaces an open one.
        sqlx::query(
            "UPDATE proposals SET status = 'expired' WHERE todo_id = ?1 AND field = ?2 AND status = 'open'",
        )
        .bind(todo_id.to_string())
        .bind(field)
        .execute(self.conn())
        .await?;
        sqlx::query(
            "INSERT INTO proposals (id, todo_id, field, value, rationale, status, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'open', ?6, ?7)",
        )
        .bind(pid.to_string())
        .bind(todo_id.to_string())
        .bind(field)
        .bind(value.to_string())
        .bind(rationale)
        .bind(ts(now()))
        .bind(ts(expires_at))
        .execute(self.conn())
        .await?;
        self.proposal(pid).await
    }

    pub async fn proposal(&mut self, pid: ProposalId) -> Result<ProposalRecord> {
        sqlx::query("SELECT * FROM proposals WHERE id = ?1")
            .bind(pid.to_string())
            .fetch_optional(self.conn())
            .await?
            .as_ref()
            .map(row_to_proposal)
            .transpose()?
            .ok_or_else(|| StoreError::NotFound(pid.to_string()))
    }

    /// Moves an open proposal to `accepted`, `rejected`, or `expired` (compare-and-set on `open`).
    pub async fn close_proposal(
        &mut self,
        pid: ProposalId,
        status: &str,
    ) -> Result<ProposalRecord> {
        let n = sqlx::query("UPDATE proposals SET status = ?1 WHERE id = ?2 AND status = 'open'")
            .bind(status)
            .bind(pid.to_string())
            .execute(self.conn())
            .await?
            .rows_affected();
        if n != 1 {
            return Err(StoreError::Conflict(format!("{pid} is not open")));
        }
        self.proposal(pid).await
    }
}

impl Store {
    pub async fn todo(&self, tid: TodoId) -> Result<Option<TodoRecord>> {
        get_todo(self.pool(), tid).await
    }

    pub async fn todos(&self, include_done: bool) -> Result<Vec<TodoRecord>> {
        let rows = sqlx::query("SELECT * FROM todos ORDER BY created_at")
            .fetch_all(self.pool())
            .await?;
        let mut todos = rows.iter().map(row_to_todo).collect::<Result<Vec<_>>>()?;
        if !include_done {
            todos.retain(|t| !matches!(t.status, TodoStatus::Done | TodoStatus::Dropped));
        }
        Ok(todos)
    }

    pub async fn open_proposals(&self) -> Result<Vec<ProposalRecord>> {
        sqlx::query("SELECT * FROM proposals WHERE status = 'open' ORDER BY created_at")
            .fetch_all(self.pool())
            .await?
            .iter()
            .map(row_to_proposal)
            .collect()
    }
}
