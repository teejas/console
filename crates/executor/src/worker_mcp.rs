//! The MCP server workers use to report back (`docs/specs/workers.md#worker-tools`).
//!
//! Served on a local port; each worker's requests carry its task in [`TASK_HEADER`].

use std::str::FromStr as _;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use console_proto::TaskId;
use console_proto::link::{ExecutorMsg, WorkerUpdate};
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;

use crate::Executor;

/// Identifies the worker's task on every MCP request.
pub const TASK_HEADER: &str = "x-console-task";

#[derive(Debug, Clone)]
pub struct WorkerTools {
    exec: Arc<Executor>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct Note {
    /// A short progress note.
    pub note: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct Question {
    /// What you need from the user: a decision or information you can't find.
    pub question: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct Completion {
    /// What you did, in a few sentences.
    pub summary: String,
    /// Evidence that the acceptance criteria are met: checks run and their results.
    #[serde(default)]
    pub evidence: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct Followup {
    /// A short title for the follow-up work.
    pub title: String,
    /// Why it matters.
    #[serde(default)]
    pub why: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct Blocked {
    /// What blocks you.
    pub reason: String,
}

type ToolResult = Result<String, String>;

impl WorkerTools {
    fn ctl(&self, parts: &http::request::Parts) -> Result<Arc<crate::workers::WorkerCtl>, String> {
        let task = parts
            .headers
            .get(TASK_HEADER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| TaskId::from_str(v).ok())
            .ok_or("missing task header")?;
        self.exec
            .workers
            .get(task)
            .ok_or_else(|| format!("task {task} has no running worker"))
    }
}

#[tool_router(server_handler)]
impl WorkerTools {
    #[must_use]
    pub fn new(exec: Arc<Executor>) -> Self {
        Self { exec }
    }

    #[tool(description = "Report a notable milestone to the user's Orchestrator.")]
    async fn report_progress(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(p): Parameters<Note>,
    ) -> ToolResult {
        let ctl = self.ctl(&parts)?;
        self.exec.send(&ExecutorMsg::Progress {
            command_id: ctl.command_id,
            update: WorkerUpdate::Text {
                text: format!("\n📝 {}\n", p.note),
            },
        });
        Ok("Noted.".into())
    }

    #[tool(
        description = "Ask the user a question you can't answer yourself. Then end your turn; the answer arrives as your next message."
    )]
    async fn ask_orchestrator(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(p): Parameters<Question>,
    ) -> ToolResult {
        let ctl = self.ctl(&parts)?;
        ctl.ask(&self.exec, p.question);
        Ok("Your question is with the user. End your turn now; the answer will arrive as your next message.".into())
    }

    #[tool(
        description = "Declare the task done, once every acceptance criterion is met. Then end your turn."
    )]
    async fn complete(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(p): Parameters<Completion>,
    ) -> ToolResult {
        let ctl = self.ctl(&parts)?;
        let summary = if p.evidence.trim().is_empty() {
            p.summary
        } else {
            format!("{}\n\nEvidence: {}", p.summary, p.evidence)
        };
        ctl.complete(summary);
        Ok("Recorded. End your turn now.".into())
    }

    #[tool(
        description = "Propose related follow-up work for the user's queue, instead of doing it now."
    )]
    async fn propose_followup(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(p): Parameters<Followup>,
    ) -> ToolResult {
        let ctl = self.ctl(&parts)?;
        let args = serde_json::json!({ "title": p.title, "why": p.why });
        self.exec
            .mcp_call(
                ctl.command_id,
                "propose_followup",
                args,
                Duration::from_secs(15),
            )
            .await
    }

    #[tool(description = "Report that you're blocked and need the user. Then end your turn.")]
    async fn mark_blocked(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(p): Parameters<Blocked>,
    ) -> ToolResult {
        let ctl = self.ctl(&parts)?;
        ctl.ask(&self.exec, format!("The worker is blocked: {}", p.reason));
        Ok(
            "The user has been told. End your turn now; their answer arrives as your next message."
                .into(),
        )
    }
}

async fn require_token(State(token): State<Arc<String>>, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.strip_prefix("Bearer ") == Some(token.as_str()));
    if ok {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, "bad token").into_response()
    }
}

/// Serves the worker tools on a local port and returns its URL.
pub async fn serve(exec: Arc<Executor>) -> std::io::Result<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/mcp", listener.local_addr()?);
    let token = Arc::new(exec.config.token()?);
    let e2 = exec.clone();
    let service = StreamableHttpService::new(
        move || Ok(WorkerTools::new(e2.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let router = Router::new()
        .nest_service("/mcp", service)
        .layer(axum::middleware::from_fn_with_state(token, require_token));
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Ok(url)
}
