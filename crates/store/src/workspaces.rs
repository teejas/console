//! Executors and the workspaces they own.

use chrono::{DateTime, Utc};
use console_proto::event::EventBody;
use console_proto::link::{Capability, WorkspaceInfo};
use console_proto::{ExecutorId, WorkspaceId};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::{EventKeys, Result, Store, WriteTx, from_json, id, json, now, parse_ts, ts};

#[derive(Debug, Clone, PartialEq)]
pub struct ExecutorRecord {
    pub id: ExecutorId,
    pub name: String,
    pub online: bool,
    pub capabilities: Vec<Capability>,
    pub last_seen: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkspaceRecord {
    pub id: WorkspaceId,
    pub executor_id: ExecutorId,
    pub name: String,
    pub path: String,
    pub repo_remote: Option<String>,
    pub head_rev: Option<String>,
    pub dirty: bool,
    pub updated_at: DateTime<Utc>,
}

fn row_to_executor(row: &SqliteRow) -> Result<ExecutorRecord> {
    let online: i64 = row.try_get("online")?;
    Ok(ExecutorRecord {
        id: id(&row.try_get::<String, _>("id")?)?,
        name: row.try_get("name")?,
        online: online != 0,
        capabilities: from_json(&row.try_get::<String, _>("capabilities")?)?,
        last_seen: parse_ts(&row.try_get::<String, _>("last_seen")?)?,
    })
}

fn row_to_workspace(row: &SqliteRow) -> Result<WorkspaceRecord> {
    let dirty: i64 = row.try_get("dirty")?;
    Ok(WorkspaceRecord {
        id: id(&row.try_get::<String, _>("id")?)?,
        executor_id: id(&row.try_get::<String, _>("executor_id")?)?,
        name: row.try_get("name")?,
        path: row.try_get("path")?,
        repo_remote: row.try_get("repo_remote")?,
        head_rev: row.try_get("head_rev")?,
        dirty: dirty != 0,
        updated_at: parse_ts(&row.try_get::<String, _>("updated_at")?)?,
    })
}

impl WriteTx {
    /// Records an executor connecting, with its capabilities and workspaces, and appends
    /// `ExecutorStatus { online: true }`.
    pub async fn executor_online(
        &mut self,
        eid: ExecutorId,
        name: &str,
        capabilities: &[Capability],
        workspaces: &[WorkspaceInfo],
    ) -> Result<()> {
        let at = ts(now());
        sqlx::query(
            "INSERT INTO executors (id, name, online, capabilities, last_seen) VALUES (?1, ?2, 1, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET name = ?2, online = 1, capabilities = ?3, last_seen = ?4",
        )
        .bind(eid.to_string())
        .bind(name)
        .bind(json(&capabilities))
        .bind(&at)
        .execute(self.conn())
        .await?;
        for w in workspaces {
            sqlx::query(
                "INSERT INTO workspaces (id, executor_id, name, path, repo_remote, head_rev, dirty, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(id) DO UPDATE SET executor_id = ?2, name = ?3, path = ?4,
                     repo_remote = ?5, head_rev = ?6, dirty = ?7, updated_at = ?8",
            )
            .bind(w.workspace_id.to_string())
            .bind(eid.to_string())
            .bind(&w.name)
            .bind(&w.path)
            .bind(&w.repo_remote)
            .bind(&w.head_rev)
            .bind(i64::from(w.dirty))
            .bind(&at)
            .execute(self.conn())
            .await?;
        }
        self.append(
            EventKeys::default(),
            EventBody::ExecutorStatus {
                executor_id: eid,
                online: true,
            },
        )
        .await?;
        Ok(())
    }

    pub async fn executor_offline(&mut self, eid: ExecutorId) -> Result<()> {
        sqlx::query("UPDATE executors SET online = 0, last_seen = ?1 WHERE id = ?2")
            .bind(ts(now()))
            .bind(eid.to_string())
            .execute(self.conn())
            .await?;
        self.append(
            EventKeys::default(),
            EventBody::ExecutorStatus {
                executor_id: eid,
                online: false,
            },
        )
        .await?;
        Ok(())
    }

    /// Marks every executor offline (at service start, before any reconnects).
    pub async fn all_executors_offline(&mut self) -> Result<()> {
        sqlx::query("UPDATE executors SET online = 0")
            .execute(self.conn())
            .await?;
        Ok(())
    }
}

impl Store {
    pub async fn executors(&self) -> Result<Vec<ExecutorRecord>> {
        sqlx::query("SELECT * FROM executors ORDER BY name")
            .fetch_all(self.pool())
            .await?
            .iter()
            .map(row_to_executor)
            .collect()
    }

    pub async fn workspaces(&self) -> Result<Vec<WorkspaceRecord>> {
        sqlx::query("SELECT * FROM workspaces ORDER BY name")
            .fetch_all(self.pool())
            .await?
            .iter()
            .map(row_to_workspace)
            .collect()
    }

    pub async fn workspace(&self, wid: WorkspaceId) -> Result<Option<WorkspaceRecord>> {
        sqlx::query("SELECT * FROM workspaces WHERE id = ?1")
            .bind(wid.to_string())
            .fetch_optional(self.pool())
            .await?
            .as_ref()
            .map(row_to_workspace)
            .transpose()
    }

    /// Looks a workspace up by its name or ID string.
    pub async fn find_workspace(&self, name_or_id: &str) -> Result<Option<WorkspaceRecord>> {
        sqlx::query("SELECT * FROM workspaces WHERE name = ?1 OR id = ?1 LIMIT 1")
            .bind(name_or_id)
            .fetch_optional(self.pool())
            .await?
            .as_ref()
            .map(row_to_workspace)
            .transpose()
    }
}
