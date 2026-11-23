//! The append-only event log.

use console_proto::event::{Event, EventBody};
use console_proto::{EventSeq, SessionId, TaskId};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::{
    EventKeys, Result, Store, StoreError, WriteTx, from_json, json, now, opt_id, parse_ts, ts,
};

/// Which events a reader or subscriber sees.
#[derive(Debug, Clone, PartialEq)]
pub enum EventFilter {
    All,
    /// The Orchestrator conversation: its own events plus unscoped ones (task transitions,
    /// decisions, queue changes, executor status).
    Conversation(SessionId),
    /// A worker session: its own events plus everything about its task.
    Worker {
        session_id: SessionId,
        task_id: TaskId,
    },
}

impl EventFilter {
    #[must_use]
    pub fn matches(&self, e: &Event) -> bool {
        match self {
            Self::All => true,
            Self::Conversation(s) => e.session_id.is_none() || e.session_id.as_ref() == Some(s),
            Self::Worker {
                session_id,
                task_id,
            } => e.session_id.as_ref() == Some(session_id) || e.task_id.as_ref() == Some(task_id),
        }
    }
}

fn kind_of(body: &EventBody) -> String {
    serde_json::to_value(body)
        .ok()
        .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

fn row_to_event(row: &SqliteRow) -> Result<Event> {
    let seq: i64 = row.try_get("seq")?;
    let at: String = row.try_get("at")?;
    let payload: String = row.try_get("payload")?;
    Ok(Event {
        seq: EventSeq(u64::try_from(seq).map_err(|_| StoreError::Corrupt("negative seq".into()))?),
        at: parse_ts(&at)?,
        session_id: opt_id(row.try_get("session_id")?)?,
        task_id: opt_id(row.try_get("task_id")?)?,
        todo_id: opt_id(row.try_get("todo_id")?)?,
        body: from_json(&payload)?,
    })
}

fn seq_param(seq: EventSeq) -> i64 {
    i64::try_from(seq.0).unwrap_or(i64::MAX)
}

impl WriteTx {
    /// Appends an event in this transaction. It is broadcast after commit.
    pub async fn append(&mut self, keys: EventKeys, body: EventBody) -> Result<Event> {
        let at = now();
        let row = sqlx::query(
            "INSERT INTO events (at, session_id, task_id, todo_id, kind, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) RETURNING seq",
        )
        .bind(ts(at))
        .bind(keys.session_id.as_ref().map(ToString::to_string))
        .bind(keys.task_id.as_ref().map(ToString::to_string))
        .bind(keys.todo_id.as_ref().map(ToString::to_string))
        .bind(kind_of(&body))
        .bind(json(&body))
        .fetch_one(self.conn())
        .await?;
        let seq: i64 = row.try_get("seq")?;
        let event = Event {
            seq: EventSeq(u64::try_from(seq).unwrap_or_default()),
            at,
            session_id: keys.session_id,
            task_id: keys.task_id,
            todo_id: keys.todo_id,
            body,
        };
        self.events.push(event.clone());
        Ok(event)
    }
}

impl Store {
    /// Events after `after`, oldest first, at most `limit`.
    pub async fn events_after(
        &self,
        after: EventSeq,
        filter: &EventFilter,
        limit: u32,
    ) -> Result<Vec<Event>> {
        let rows = match filter {
            EventFilter::All => {
                sqlx::query("SELECT * FROM events WHERE seq > ?1 ORDER BY seq LIMIT ?2")
                    .bind(seq_param(after))
                    .bind(limit)
                    .fetch_all(self.pool())
                    .await?
            }
            EventFilter::Conversation(s) => sqlx::query(
                "SELECT * FROM events WHERE seq > ?1 AND (session_id IS NULL OR session_id = ?2)
                     ORDER BY seq LIMIT ?3",
            )
            .bind(seq_param(after))
            .bind(s.to_string())
            .bind(limit)
            .fetch_all(self.pool())
            .await?,
            EventFilter::Worker {
                session_id,
                task_id,
            } => {
                sqlx::query(
                    "SELECT * FROM events WHERE seq > ?1 AND (session_id = ?2 OR task_id = ?3)
                     ORDER BY seq LIMIT ?4",
                )
                .bind(seq_param(after))
                .bind(session_id.to_string())
                .bind(task_id.to_string())
                .bind(limit)
                .fetch_all(self.pool())
                .await?
            }
        };
        rows.iter().map(row_to_event).collect()
    }

    /// The most recent `limit` matching events, oldest first.
    pub async fn recent_events(&self, filter: &EventFilter, limit: u32) -> Result<Vec<Event>> {
        let rows = match filter {
            EventFilter::All => {
                sqlx::query("SELECT * FROM events ORDER BY seq DESC LIMIT ?1")
                    .bind(limit)
                    .fetch_all(self.pool())
                    .await?
            }
            EventFilter::Conversation(s) => {
                sqlx::query(
                    "SELECT * FROM events WHERE session_id IS NULL OR session_id = ?1
                     ORDER BY seq DESC LIMIT ?2",
                )
                .bind(s.to_string())
                .bind(limit)
                .fetch_all(self.pool())
                .await?
            }
            EventFilter::Worker {
                session_id,
                task_id,
            } => {
                sqlx::query(
                    "SELECT * FROM events WHERE session_id = ?1 OR task_id = ?2
                     ORDER BY seq DESC LIMIT ?3",
                )
                .bind(session_id.to_string())
                .bind(task_id.to_string())
                .bind(limit)
                .fetch_all(self.pool())
                .await?
            }
        };
        let mut events = rows.iter().map(row_to_event).collect::<Result<Vec<_>>>()?;
        events.reverse();
        Ok(events)
    }

    /// The newest sequence number, or `EventSeq::START` for an empty log.
    pub async fn last_seq(&self) -> Result<EventSeq> {
        let row = sqlx::query("SELECT COALESCE(MAX(seq), 0) AS seq FROM events")
            .fetch_one(self.pool())
            .await?;
        let seq: i64 = row.try_get("seq")?;
        Ok(EventSeq(u64::try_from(seq).unwrap_or_default()))
    }
}
