//! M0 spike (b): host Claude Code as a *worker* through its ACP adapter, from Rust.
//!
//! - Spawns `@agentclientprotocol/claude-agent-acp` (override with $WORKER_CMD).
//! - Creates a session in a workspace with our MCP stub attached (stdio).
//! - Sends one prompt, logs every session update compactly, auto-approves permissions
//!   (logging what was asked: in the product these become decisions).
//! - Optional: $CANCEL_AFTER_SECS sends session/cancel mid-turn to check the stop reason.
//!
//! Usage: acp-host-claude <workspace-dir> <mcp-stub-path> <prompt>

use std::{
    path::PathBuf,
    str::FromStr,
    time::{Duration, Instant},
};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    CancelNotification, ContentBlock, EnvVariable, InitializeRequest, McpServer, McpServerStdio,
    NewSessionRequest, PermissionOptionKind, PromptRequest, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
    SessionNotification, SessionUpdate, TextContent,
};
use agent_client_protocol::{AcpAgent, Agent};

fn short(s: &str, n: usize) -> String {
    let s = s.replace('\n', " ⏎ ");
    if s.chars().count() > n {
        format!("{}…", s.chars().take(n).collect::<String>())
    } else {
        s
    }
}

fn describe(update: &SessionUpdate) -> Option<String> {
    Some(match update {
        SessionUpdate::AgentMessageChunk(c) => match &c.content {
            ContentBlock::Text(t) => format!("msg  {}", short(&t.text, 120)),
            _ => "msg  <non-text>".into(),
        },
        SessionUpdate::AgentThoughtChunk(_) => return None,
        SessionUpdate::ToolCall(t) => {
            format!("tool {} [{:?}] {:?}", short(&t.title, 90), t.kind, t.status)
        }
        SessionUpdate::ToolCallUpdate(u) => format!(
            "tool~ {} status={:?} title={:?}",
            u.tool_call_id,
            u.fields.status,
            u.fields.title.as_deref().map(|t| short(t, 60))
        ),
        SessionUpdate::Plan(p) => format!("plan {} entries", p.entries.len()),
        SessionUpdate::AvailableCommandsUpdate(c) => {
            format!("cmds {} available", c.available_commands.len())
        }
        SessionUpdate::UsageUpdate(u) => format!("usage used={} size={}", u.used, u.size),
        other => format!("{}", short(&format!("{other:?}"), 100)),
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let workspace = PathBuf::from(&args[1]).canonicalize()?;
    let mcp_stub = PathBuf::from(&args[2]).canonicalize()?;
    let prompt = args[3].clone();
    let cmd = std::env::var("WORKER_CMD")
        .unwrap_or_else(|_| "npx -y @agentclientprotocol/claude-agent-acp".to_string());
    let mcp_log = std::env::var("MCP_LOG").unwrap_or_else(|_| "/dev/null".into());
    let cancel_after = std::env::var("CANCEL_AFTER_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok());

    let t0 = Instant::now();
    let ts = move || format!("{:>6.1}s", t0.elapsed().as_secs_f32());
    eprintln!("{} spawning: {cmd}", ts());
    let agent = AcpAgent::from_str(&cmd)?;

    agent_client_protocol::Client
        .v1()
        .on_receive_notification(
            async move |n: SessionNotification, _cx| {
                if let Some(line) = describe(&n.update) {
                    println!("{} {line}", ts());
                }
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |req: RequestPermissionRequest, responder, _cx| {
                let kinds: Vec<_> = req
                    .options
                    .iter()
                    .map(|o| format!("{:?}", o.kind))
                    .collect();
                println!(
                    "{} PERMISSION {:?} options={kinds:?}",
                    ts(),
                    req.tool_call.fields.title.as_deref().map(|t| short(t, 80))
                );
                let id = [
                    PermissionOptionKind::AllowOnce,
                    PermissionOptionKind::AllowAlways,
                ]
                .iter()
                .find_map(|k| {
                    req.options
                        .iter()
                        .find(|o| o.kind == *k)
                        .map(|o| o.option_id.clone())
                });
                match id {
                    Some(id) => responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)),
                    )),
                    None => responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    )),
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(agent, |cx: ConnectionTo<Agent>| async move {
            let init = cx
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            println!(
                "{} initialized agent={:?} load_session={}",
                ts(),
                init.agent_info
                    .as_ref()
                    .map(|i| format!("{} {}", i.name, i.version)),
                init.agent_capabilities.load_session
            );

            let mcp = McpServer::Stdio(
                McpServerStdio::new("orch", mcp_stub.clone())
                    .env(vec![EnvVariable::new("MCP_LOG", mcp_log.clone())]),
            );
            let session = cx
                .send_request(NewSessionRequest::new(workspace.clone()).mcp_servers(vec![mcp]))
                .block_task()
                .await?;
            let sid = session.session_id.clone();
            println!("{} session {sid}", ts());

            if let Some(secs) = cancel_after {
                let cx2 = cx.clone();
                let sid2 = sid.clone();
                cx.spawn(async move {
                    tokio::time::sleep(Duration::from_secs(secs)).await;
                    println!("{} >>> sending session/cancel", ts());
                    cx2.send_notification(CancelNotification::new(sid2))
                })?;
            }

            let resp = cx
                .send_request(PromptRequest::new(
                    sid,
                    vec![ContentBlock::Text(TextContent::new(prompt))],
                ))
                .block_task()
                .await?;
            println!("{} DONE stop_reason={:?}", ts(), resp.stop_reason);
            Ok(())
        })
        .await?;
    Ok(())
}
