//! The Orchestrator agent's MCP tool server (`/mcp`, streamable HTTP).
//!
//! Workspace tools are read-only by construction: they become executor read ops. The only
//! way work gets done in a workspace is `delegate_task`.

use std::str::FromStr as _;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use console_agent::Brief;
use console_proto::api::{TodoAddParams, TodoUpdateParams};
use console_proto::domain::{Score, TodoKind, TodoStatus};
use console_proto::link::{ReadOp, ReadOpOutcome};
use console_proto::{DecisionId, TaskId, TodoId, WorkspaceId};
use console_queue::Actor;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;

use crate::task_machine::Input;
use crate::{App, engine, queue_ops, work};

const READ_DEADLINE: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub struct ConsoleTools {
    app: Arc<App>,
}

type ToolResult = Result<String, String>;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WsPath {
    /// Workspace name.
    pub workspace: String,
    /// Path relative to the workspace root. Empty for the root.
    #[serde(default)]
    pub path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WsFile {
    pub workspace: String,
    /// Path relative to the workspace root.
    pub path: String,
    /// Maximum bytes to return (default 32768).
    #[serde(default)]
    pub max_bytes: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WsSearch {
    pub workspace: String,
    /// Regular expression, matched per line.
    pub pattern: String,
    /// Only files matching this glob, for example `src/**/*.rs`.
    #[serde(default)]
    pub glob: Option<String>,
    /// Maximum matches (default 50).
    #[serde(default)]
    pub max_matches: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct Ws {
    pub workspace: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WsLog {
    pub workspace: String,
    /// Number of commits (default 20).
    #[serde(default)]
    pub max: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WsDiff {
    pub workspace: String,
    /// For example `main..HEAD`. Empty for uncommitted changes.
    #[serde(default)]
    pub rev_range: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WsShow {
    pub workspace: String,
    /// A commit, branch, or tag.
    pub rev: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct WsRun {
    pub workspace: String,
    /// The command and its arguments; it must be on the read-only allowlist.
    pub argv: Vec<String>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct QueueQuery {
    /// Include done and dropped items.
    #[serde(default)]
    pub include_done: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TodoNew {
    pub title: String,
    /// Longer description.
    #[serde(default)]
    pub body: Option<String>,
    /// feature, bugfix, review, investigation, ops, chore, or question.
    #[serde(default)]
    pub kind: Option<String>,
    /// 1 (low) to 4 (high).
    #[serde(default)]
    pub importance: Option<u8>,
    /// 1 (low) to 4 (high).
    #[serde(default)]
    pub urgency: Option<u8>,
    /// Due this many hours from now.
    #[serde(default)]
    pub due_in_hours: Option<i64>,
    /// Workspace name, if the todo is about one.
    #[serde(default)]
    pub workspace: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TodoEdit {
    /// The todo's ID (from queue_list).
    pub todo_id: String,
    /// Why you're making this change. Shown to the user if it becomes a proposal.
    pub rationale: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub importance: Option<u8>,
    #[serde(default)]
    pub urgency: Option<u8>,
    /// ready, active, blocked, waiting_on_user, done, or dropped.
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub due_in_hours: Option<i64>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct TaskQuery {
    /// One task's ID. Omit for all active tasks.
    #[serde(default)]
    pub task_id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TaskRef {
    pub task_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TaskMessage {
    pub task_id: String,
    /// What to tell the worker: an answer, a correction, or new direction.
    pub text: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DecisionAnswer {
    pub decision_id: String,
    /// The chosen option's ID, for decisions with options.
    #[serde(default)]
    pub option_id: Option<String>,
    /// A free-text answer, for worker questions.
    #[serde(default)]
    pub answer: Option<String>,
}

fn enum_from<T: serde::de::DeserializeOwned>(s: &str, what: &str) -> Result<T, String> {
    serde_json::from_value(serde_json::Value::String(s.trim().to_lowercase()))
        .map_err(|_| format!("{s:?} is not a valid {what}"))
}

fn score(v: Option<u8>) -> Result<Option<Score>, String> {
    v.map(Score::try_from).transpose()
}

impl ConsoleTools {
    async fn workspace(&self, name: &str) -> Result<WorkspaceId, String> {
        match self
            .app
            .store
            .find_workspace(name.trim())
            .await
            .map_err(|e| e.to_string())?
        {
            Some(w) => Ok(w.id),
            None => {
                let names: Vec<String> = self
                    .app
                    .store
                    .workspaces()
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|w| w.name)
                    .collect();
                Err(format!(
                    "unknown workspace {name:?}; known: {}",
                    names.join(", ")
                ))
            }
        }
    }

    async fn read(&self, workspace: WorkspaceId, op: ReadOp) -> ToolResult {
        match self
            .app
            .hub
            .read_op(&self.app, workspace, op, READ_DEADLINE)
            .await
        {
            Ok(ReadOpOutcome::Ok { output, truncated }) => Ok(if truncated {
                format!("{output}\n[output truncated]")
            } else {
                output
            }),
            Ok(ReadOpOutcome::Denied { reason }) => Err(format!("denied: {reason}")),
            Ok(ReadOpOutcome::Failed { error }) => Err(error),
            Err(e) => Err(e.to_string()),
        }
    }
}

#[tool_router(server_handler)]
impl ConsoleTools {
    #[must_use]
    pub fn new(app: Arc<App>) -> Self {
        Self { app }
    }

    #[tool(description = "List registered workspaces and whether their executor is online.")]
    async fn workspaces(&self) -> ToolResult {
        let ws = self.app.workspace_views().await.map_err(|e| e.0.message)?;
        if ws.is_empty() {
            return Ok("No workspaces are registered. An executor must be running with workspaces configured.".into());
        }
        Ok(ws
            .iter()
            .map(|w| {
                format!(
                    "{} (executor {}, {}{}{})",
                    w.name,
                    w.executor_name,
                    if w.online { "online" } else { "offline" },
                    w.head_rev
                        .as_deref()
                        .map(|h| format!(", HEAD {}", &h[..h.len().min(8)]))
                        .unwrap_or_default(),
                    if w.dirty { ", uncommitted changes" } else { "" }
                )
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }

    #[tool(description = "List a directory in a workspace (read-only).")]
    async fn ws_list_dir(&self, Parameters(p): Parameters<WsPath>) -> ToolResult {
        let w = self.workspace(&p.workspace).await?;
        self.read(
            w,
            ReadOp::ListDir {
                workspace_id: w,
                path: p.path,
            },
        )
        .await
    }

    #[tool(description = "Read a file in a workspace (read-only, size-capped).")]
    async fn ws_read_file(&self, Parameters(p): Parameters<WsFile>) -> ToolResult {
        let w = self.workspace(&p.workspace).await?;
        self.read(
            w,
            ReadOp::ReadFile {
                workspace_id: w,
                path: p.path,
                max_bytes: p.max_bytes.unwrap_or(32_768),
            },
        )
        .await
    }

    #[tool(
        description = "Search a workspace with a regular expression; returns path:line:text matches."
    )]
    async fn ws_search(&self, Parameters(p): Parameters<WsSearch>) -> ToolResult {
        let w = self.workspace(&p.workspace).await?;
        self.read(
            w,
            ReadOp::Search {
                workspace_id: w,
                pattern: p.pattern,
                path_glob: p.glob,
                max_matches: p.max_matches.unwrap_or(50),
            },
        )
        .await
    }

    #[tool(description = "git status of a workspace.")]
    async fn git_status(&self, Parameters(p): Parameters<Ws>) -> ToolResult {
        let w = self.workspace(&p.workspace).await?;
        self.read(w, ReadOp::GitStatus { workspace_id: w }).await
    }

    #[tool(description = "Recent commits in a workspace.")]
    async fn git_log(&self, Parameters(p): Parameters<WsLog>) -> ToolResult {
        let w = self.workspace(&p.workspace).await?;
        self.read(
            w,
            ReadOp::GitLog {
                workspace_id: w,
                max: p.max.unwrap_or(20),
            },
        )
        .await
    }

    #[tool(description = "git diff in a workspace: uncommitted changes, or a revision range.")]
    async fn git_diff(&self, Parameters(p): Parameters<WsDiff>) -> ToolResult {
        let w = self.workspace(&p.workspace).await?;
        self.read(
            w,
            ReadOp::GitDiff {
                workspace_id: w,
                rev_range: p.rev_range.filter(|r| !r.is_empty()),
            },
        )
        .await
    }

    #[tool(description = "Show a commit in a workspace.")]
    async fn git_show(&self, Parameters(p): Parameters<WsShow>) -> ToolResult {
        let w = self.workspace(&p.workspace).await?;
        self.read(
            w,
            ReadOp::GitShow {
                workspace_id: w,
                rev: p.rev,
            },
        )
        .await
    }

    #[tool(
        description = "Run an allowlisted read-only command in a workspace, for example `gh pr view 42`."
    )]
    async fn run_readonly(&self, Parameters(p): Parameters<WsRun>) -> ToolResult {
        let w = self.workspace(&p.workspace).await?;
        self.read(
            w,
            ReadOp::RunReadonly {
                workspace_id: w,
                argv: p.argv,
            },
        )
        .await
    }

    #[tool(
        description = "The shared priority queue, highest priority first, with why each item sits where it does."
    )]
    async fn queue_list(&self, Parameters(p): Parameters<QueueQuery>) -> ToolResult {
        let q = queue_ops::list(&self.app, p.include_done)
            .await
            .map_err(|e| e.0.message)?;
        if q.is_empty() {
            return Ok("The queue is empty.".into());
        }
        Ok(q.iter()
            .enumerate()
            .map(|(i, t)| {
                let status = serde_json::to_value(t.status)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default();
                format!(
                    "{}. {:?} {status}: {} [{}] (importance {}, urgency {}) — {}",
                    i + 1,
                    t.band,
                    t.title,
                    t.id,
                    t.importance.get(),
                    t.urgency.get(),
                    t.why
                )
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }

    #[tool(description = "Add a todo to the shared queue. Propose kind, importance, and urgency.")]
    async fn todo_add(&self, Parameters(p): Parameters<TodoNew>) -> ToolResult {
        let workspace_id = match p.workspace.as_deref() {
            Some(w) => Some(self.workspace(w).await?),
            None => None,
        };
        let t = queue_ops::add(
            &self.app,
            TodoAddParams {
                title: p.title,
                body: p.body,
                kind: p
                    .kind
                    .as_deref()
                    .map(|k| enum_from::<TodoKind>(k, "kind"))
                    .transpose()?,
                importance: score(p.importance)?,
                urgency: score(p.urgency)?,
                due_at: p
                    .due_in_hours
                    .map(|h| Utc::now() + chrono::Duration::hours(h)),
                workspace_id,
            },
            Actor::Orchestrator,
        )
        .await
        .map_err(|e| e.0.message)?;
        Ok(format!(
            "Added [{}] {:?} {}: {}",
            t.id, t.band, t.title, t.why
        ))
    }

    #[tool(
        description = "Change a todo. Changes to fields the user set become proposals with your rationale; status changes apply directly."
    )]
    async fn todo_update(&self, Parameters(p): Parameters<TodoEdit>) -> ToolResult {
        let id = TodoId::from_str(&p.todo_id).map_err(|e| e.to_string())?;
        let current = queue_ops::get(&self.app, id)
            .await
            .map_err(|e| e.0.message)?;
        let r = queue_ops::update(
            &self.app,
            TodoUpdateParams {
                todo_id: id,
                expected_version: current.version,
                title: p.title,
                kind: p
                    .kind
                    .as_deref()
                    .map(|k| enum_from::<TodoKind>(k, "kind"))
                    .transpose()?,
                owner: None,
                status: p
                    .status
                    .as_deref()
                    .map(|s| enum_from::<TodoStatus>(s, "status"))
                    .transpose()?,
                importance: score(p.importance)?,
                urgency: score(p.urgency)?,
                due_at: p
                    .due_in_hours
                    .map(|h| Utc::now() + chrono::Duration::hours(h)),
                clear_due: false,
                workspace_id: None,
            },
            Actor::Orchestrator,
            &p.rationale,
        )
        .await
        .map_err(|e| e.0.message)?;
        let mut msg = format!(
            "Updated [{}] {:?} {}.",
            r.todo.id, r.todo.band, r.todo.title
        );
        if !r.proposals.is_empty() {
            let fields: Vec<&str> = r.proposals.iter().map(|p| p.field.as_str()).collect();
            msg.push_str(&format!(
                " The user owns {}, so those changes are proposals awaiting them.",
                fields.join(", ")
            ));
        }
        Ok(msg)
    }

    #[tool(
        description = "Hand non-trivial work to a dedicated worker agent in a workspace. Required for any workspace change, builds, tests, or multi-step investigations. Returns immediately; the worker runs in the background."
    )]
    async fn delegate_task(&self, Parameters(brief): Parameters<Brief>) -> ToolResult {
        let turn = self.app.agent.current().map(|(t, _)| t);
        work::delegate(&self.app, brief, turn).await
    }

    #[tool(
        description = "Status of active tasks, or details of one task (state, summary, patch size)."
    )]
    async fn task_status(&self, Parameters(p): Parameters<TaskQuery>) -> ToolResult {
        if let Some(id) = p.task_id {
            let id = TaskId::from_str(&id).map_err(|e| e.to_string())?;
            let t = self
                .app
                .store
                .task(id)
                .await
                .map_err(|e| e.to_string())?
                .ok_or("no such task")?;
            let patch = self
                .app
                .store
                .latest_artifact(id, "patch")
                .await
                .ok()
                .flatten();
            let mut out = format!("{} \"{}\": {:?}", t.id, t.title, t.state);
            if let Some(b) = t.blocked {
                out.push_str(&format!(" ({b:?})"));
            }
            if let Some(s) = t.summary {
                out.push_str(&format!("\nSummary: {s}"));
            }
            if let Some(p) = patch {
                let files = p
                    .content
                    .lines()
                    .filter(|l| l.starts_with("diff --git"))
                    .count();
                out.push_str(&format!(
                    "\nPatch: {files} file(s) changed ({} lines)",
                    p.content.lines().count()
                ));
            }
            return Ok(out);
        }
        let tasks = self
            .app
            .store
            .tasks(false)
            .await
            .map_err(|e| e.to_string())?;
        if tasks.is_empty() {
            return Ok("No active tasks.".into());
        }
        Ok(tasks
            .iter()
            .map(|t| {
                format!(
                    "{} \"{}\": {:?}{}",
                    t.id,
                    t.title,
                    t.state,
                    t.blocked.map(|b| format!(" ({b:?})")).unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }

    #[tool(
        description = "Send a message to a running worker: an answer, a correction, or new direction."
    )]
    async fn task_message(&self, Parameters(p): Parameters<TaskMessage>) -> ToolResult {
        let id = TaskId::from_str(&p.task_id).map_err(|e| e.to_string())?;
        self.app.message_worker(id, &p.text).await
    }

    #[tool(
        description = "Cancel a task. The worker is stopped; cancellation is confirmed asynchronously."
    )]
    async fn task_cancel(&self, Parameters(p): Parameters<TaskRef>) -> ToolResult {
        let id = TaskId::from_str(&p.task_id).map_err(|e| e.to_string())?;
        let t = engine::apply(&self.app.store, id, Input::CancelRequested)
            .await
            .map_err(|e| e.to_string())?;
        self.app.on_cancel_requested(&t).await;
        Ok(format!(
            "Cancel requested for \"{}\" (now {:?}).",
            t.title, t.state
        ))
    }

    #[tool(
        description = "Answer a pending decision on the user's behalf, only when the user told you the answer."
    )]
    async fn decision_answer(&self, Parameters(p): Parameters<DecisionAnswer>) -> ToolResult {
        let id = DecisionId::from_str(&p.decision_id).map_err(|e| e.to_string())?;
        self.app.answer_decision(id, p.option_id, p.answer).await
    }
}

async fn require_token(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    let query = std::collections::HashMap::new();
    if crate::server::authorized(&app, req.headers(), &query) {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, "bad token").into_response()
    }
}

/// The `/mcp` route, protected by the local token.
pub fn router(app: Arc<App>) -> Router<Arc<App>> {
    let factory_app = app.clone();
    let service = StreamableHttpService::new(
        move || Ok(ConsoleTools::new(factory_app.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    Router::new()
        .nest_service("/mcp", service)
        .layer(axum::middleware::from_fn_with_state(app, require_token))
}
