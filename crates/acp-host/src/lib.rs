//! ACP client that hosts workers (Claude Code, Codex) through their ACP adapters (M7).
//!
//! See `docs/specs/workers.md`. One [`run`] call is one worker: it starts the adapter (in its
//! own process group, which the ACP crate kills on shutdown), opens a session in the
//! workspace with the executor's MCP server attached, sends the brief, and then follows
//! [`Control`] messages: more prompts, cancel, or stop.

use std::path::PathBuf;
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    CancelNotification, ContentBlock, HttpHeader, InitializeRequest, McpServer, McpServerHttp,
    NewSessionRequest, PromptRequest, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome, SessionNotification, SessionUpdate,
    StopReason, TextContent, ToolCallStatus,
};
use agent_client_protocol::{AcpAgent, AcpAgentConfig, Agent};
use tokio::sync::{mpsc, oneshot};

/// How to start a worker.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// The adapter command and its arguments.
    pub command: Vec<String>,
    pub env: Vec<(String, String)>,
    /// The workspace root; the session runs here.
    pub cwd: PathBuf,
    /// The executor's MCP server for this worker.
    pub mcp: Option<McpHttp>,
}

#[derive(Debug, Clone)]
pub struct McpHttp {
    pub name: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
}

/// Worker activity.
#[derive(Debug, Clone, PartialEq)]
pub enum WorkerEvent {
    Text(String),
    ToolCall {
        id: String,
        title: String,
        kind: Option<String>,
    },
    ToolCallDone {
        id: String,
        ok: bool,
    },
    Plan(Vec<String>),
    Usage {
        used: u64,
        size: u64,
    },
    /// A prompt turn ended; the worker waits for the next [`Control`].
    TurnEnded {
        stop_reason: String,
    },
}

/// A permission request from the worker, waiting for an answer (an option ID, or `None`
/// to cancel the tool call).
#[derive(Debug)]
pub struct PermissionAsk {
    pub title: String,
    /// `(option_id, label)` pairs.
    pub options: Vec<(String, String)>,
    pub reply: oneshot::Sender<Option<String>>,
}

#[derive(Debug, Clone)]
pub enum Control {
    /// Send another prompt (an answer, or steering).
    Prompt(String),
    /// Cancel the current turn.
    Cancel,
    /// End the session and stop the worker.
    Stop,
}

fn describe_kind(k: &impl std::fmt::Debug) -> String {
    format!("{k:?}").to_lowercase()
}

fn event_of(update: &SessionUpdate) -> Option<WorkerEvent> {
    Some(match update {
        SessionUpdate::AgentMessageChunk(c) => match &c.content {
            ContentBlock::Text(t) => WorkerEvent::Text(t.text.clone()),
            _ => return None,
        },
        SessionUpdate::ToolCall(t) => match t.status {
            ToolCallStatus::Completed | ToolCallStatus::Failed => WorkerEvent::ToolCallDone {
                id: t.tool_call_id.to_string(),
                ok: t.status == ToolCallStatus::Completed,
            },
            _ => WorkerEvent::ToolCall {
                id: t.tool_call_id.to_string(),
                title: t.title.clone(),
                kind: Some(describe_kind(&t.kind)),
            },
        },
        SessionUpdate::ToolCallUpdate(u) => match u.fields.status {
            Some(ToolCallStatus::Completed) => WorkerEvent::ToolCallDone {
                id: u.tool_call_id.to_string(),
                ok: true,
            },
            Some(ToolCallStatus::Failed) => WorkerEvent::ToolCallDone {
                id: u.tool_call_id.to_string(),
                ok: false,
            },
            _ => WorkerEvent::ToolCall {
                id: u.tool_call_id.to_string(),
                title: u.fields.title.clone()?,
                kind: None,
            },
        },
        SessionUpdate::Plan(p) => {
            WorkerEvent::Plan(p.entries.iter().map(|e| e.content.clone()).collect())
        }
        SessionUpdate::UsageUpdate(u) => WorkerEvent::Usage {
            used: u.used,
            size: u.size,
        },
        _ => return None,
    })
}

/// Runs one worker until it is stopped or its process ends.
pub async fn run(
    cfg: WorkerConfig,
    first_prompt: String,
    events: mpsc::UnboundedSender<WorkerEvent>,
    asks: mpsc::UnboundedSender<PermissionAsk>,
    mut control: mpsc::UnboundedReceiver<Control>,
) -> Result<(), String> {
    let (program, args) = cfg.command.split_first().ok_or("empty worker command")?;
    let mut agent_cfg = AcpAgentConfig::new(program).args(args.iter().cloned());
    for (k, v) in &cfg.env {
        agent_cfg = agent_cfg.env(k.clone(), v.clone());
    }
    let agent = AcpAgent::new(agent_cfg);
    let ev_tx = events.clone();
    let cwd = cfg.cwd.clone();
    let mcp = cfg.mcp.clone();
    agent_client_protocol::Client
        .v1()
        .on_receive_notification(
            async move |n: SessionNotification, _cx| {
                if let Some(e) = event_of(&n.update) {
                    let _ = ev_tx.send(e);
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |req: RequestPermissionRequest, responder, cx: ConnectionTo<Agent>| {
                let (tx, rx) = oneshot::channel();
                let options: Vec<(String, String)> = req
                    .options
                    .iter()
                    .map(|o| (o.option_id.to_string(), o.name.clone()))
                    .collect();
                let title = req
                    .tool_call
                    .fields
                    .title
                    .clone()
                    .unwrap_or_else(|| "a tool call".into());
                let _ = asks.send(PermissionAsk {
                    title,
                    options,
                    reply: tx,
                });
                cx.spawn(async move {
                    let outcome = match rx.await {
                        Ok(Some(id)) => {
                            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id))
                        }
                        _ => RequestPermissionOutcome::Cancelled,
                    };
                    responder.respond(RequestPermissionResponse::new(outcome))
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, |cx: ConnectionTo<Agent>| async move {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let mut servers = Vec::new();
            if let Some(m) = mcp {
                servers.push(McpServer::Http(
                    McpServerHttp::new(m.name, m.url).headers(
                        m.headers
                            .into_iter()
                            .map(|(k, v)| HttpHeader::new(k, v))
                            .collect(),
                    ),
                ));
            }
            let session = cx
                .send_request(NewSessionRequest::new(cwd).mcp_servers(servers))
                .block_task()
                .await?;
            let sid = session.session_id.clone();
            let mut next = Some(first_prompt);
            while let Some(text) = next.take() {
                let prompt = cx.send_request(PromptRequest::new(
                    sid.clone(),
                    vec![ContentBlock::Text(TextContent::new(text))],
                ));
                let mut prompt = std::pin::pin!(prompt.block_task());
                let stop = loop {
                    tokio::select! {
                        r = &mut prompt => break r?.stop_reason,
                        c = control.recv() => match c {
                            Some(Control::Cancel | Control::Stop) | None => {
                                let _ = cx.send_notification(CancelNotification::new(sid.clone()));
                            }
                            // A prompt that arrives mid-turn waits for the turn to end.
                            Some(Control::Prompt(p)) => next = Some(p),
                        },
                    }
                };
                let _ = events.send(WorkerEvent::TurnEnded {
                    stop_reason: match stop {
                        StopReason::EndTurn => "end_turn".into(),
                        StopReason::Cancelled => "cancelled".into(),
                        other => format!("{other:?}").to_lowercase(),
                    },
                });
                if stop == StopReason::Cancelled {
                    break;
                }
                if next.is_none() {
                    next = match control.recv().await {
                        Some(Control::Prompt(p)) => Some(p),
                        Some(Control::Cancel | Control::Stop) | None => None,
                    };
                }
            }
            // Give the adapter a moment to flush before the connection closes.
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(())
        })
        .await
        .map_err(|e| e.to_string())
}
