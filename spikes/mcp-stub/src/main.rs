//! M0 spike: a stub of the Orchestrator's MCP tool server (stdio).
//!
//! Tools mirror the planned product surface: an inline read-only workspace lookup,
//! a queue read, and `delegate_task`. Every call is appended to $MCP_LOG so the
//! spike can check which tools the agent actually used.

use std::io::Write as _;

use rmcp::{
    ServiceExt,
    handler::server::wrapper::{Json, Parameters},
    schemars, tool, tool_router,
};
use serde::{Deserialize, Serialize};

struct Orch;

fn log(line: String) {
    eprintln!("[mcp-stub] {line}");
    if let Ok(path) = std::env::var("MCP_LOG") {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{line}");
        }
    }
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
struct ReadFile {
    /// Workspace name, e.g. "repo-x".
    workspace: String,
    /// Path relative to the workspace root.
    path: String,
}

#[derive(Deserialize, schemars::JsonSchema, Default)]
struct Delegate {
    /// Short title for the task.
    title: String,
    /// What the worker should accomplish.
    goal: String,
    /// Workspace name, e.g. "repo-x".
    workspace: String,
    /// Criteria the worker must meet before calling the task done.
    acceptance_criteria: Vec<String>,
}

#[derive(Serialize, schemars::JsonSchema)]
struct Delegated {
    task_id: String,
    todo_id: String,
    status: String,
}

#[derive(Serialize, schemars::JsonSchema)]
struct QueueItem {
    id: String,
    band: String,
    title: String,
    status: String,
}

#[derive(Serialize, schemars::JsonSchema)]
struct Queue {
    items: Vec<QueueItem>,
}

#[tool_router(server_handler)]
impl Orch {
    #[tool(description = "Read a file from a registered workspace (read-only).")]
    fn ws_read_file(&self, Parameters(p): Parameters<ReadFile>) -> String {
        log(format!(
            "ws_read_file workspace={} path={}",
            p.workspace, p.path
        ));
        format!(
            "// {}/{}\nfn verify_token(t: &str) -> bool {{\n    // TODO: flaky under clock skew\n    t.len() > 10\n}}\n",
            p.workspace, p.path
        )
    }

    #[tool(description = "List the shared priority queue, highest priority first.")]
    fn queue_list(&self) -> Json<Queue> {
        log("queue_list".into());
        let item = |id: &str, band: &str, title: &str, status: &str| QueueItem {
            id: id.into(),
            band: band.into(),
            title: title.into(),
            status: status.into(),
        };
        Json(Queue {
            items: vec![
                item("td-1", "P0", "Fix red main build (repo-x)", "active"),
                item("td-2", "P1", "Answer review request on PR 42", "ready"),
                item("td-3", "P2", "Refactor auth middleware", "ready"),
                item("td-4", "P3", "Bump tokio", "inbox"),
            ],
        })
    }

    #[tool(
        description = "Hand non-trivial work to a dedicated worker agent. Required for any change to a workspace. Returns immediately; the worker runs in the background."
    )]
    fn delegate_task(&self, Parameters(p): Parameters<Delegate>) -> Json<Delegated> {
        log(format!(
            "delegate_task title={:?} workspace={} criteria={} goal={:?}",
            p.title,
            p.workspace,
            p.acceptance_criteria.len(),
            p.goal
        ));
        Json(Delegated {
            task_id: "T-123".into(),
            todo_id: "td-9".into(),
            status: "queued".into(),
        })
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    log(format!("started pid={}", std::process::id()));
    let service = Orch.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
