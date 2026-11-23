//! M0 spike (a): a stub "Orchestrator" ACP v1 agent for probing Toad.
//!
//! What it exercises:
//! - `loadSession`: history is persisted to a JSON file and replayed on `session/load`.
//! - Single long-lived conversation: every `session/new` maps to the same session id,
//!   and replays history during `session/new` (does Toad render it?).
//! - Slash commands via `available_commands_update`.
//! - Plan entries (the queue mirror) via `plan` updates.
//! - Tool calls (inline lookups / delegation) via `tool_call` + `tool_call_update`.
//! - Out-of-turn updates: `/notify N` schedules a message and a plan change N seconds
//!   after the prompt has already finished.
//!
//! stdout is the JSON-RPC channel; diagnostics go to stderr and to $STUB_LOG.

use std::{
    io::Write as _,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    AgentCapabilities, AvailableCommand, AvailableCommandsUpdate, CancelNotification, ContentBlock,
    ContentChunk, InitializeRequest, InitializeResponse, LoadSessionRequest, LoadSessionResponse,
    NewSessionRequest, NewSessionResponse, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus,
    PromptRequest, PromptResponse, SessionNotification, SessionUpdate, StopReason, ToolCall,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{Agent, Client, Result, Stdio};
use serde::{Deserialize, Serialize};

const SESSION_ID: &str = "orch-main";

#[derive(Clone, Serialize, Deserialize)]
struct Turn {
    role: String, // "user" | "agent"
    text: String,
}

#[derive(Clone)]
struct State {
    history: Arc<Mutex<Vec<Turn>>>,
    path: PathBuf,
    tool_seq: Arc<Mutex<u64>>,
}

impl State {
    fn load(path: PathBuf) -> Self {
        let history = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            history: Arc::new(Mutex::new(history)),
            path,
            tool_seq: Arc::new(Mutex::new(0)),
        }
    }
    fn push(&self, role: &str, text: &str) {
        let mut h = self.history.lock().unwrap();
        h.push(Turn {
            role: role.into(),
            text: text.into(),
        });
        let _ = std::fs::write(&self.path, serde_json::to_vec_pretty(&*h).unwrap());
    }
    fn next_tool_id(&self) -> String {
        let mut s = self.tool_seq.lock().unwrap();
        *s += 1;
        format!("tool-{}-{}", std::process::id(), *s)
    }
}

fn log(msg: impl AsRef<str>) {
    eprintln!("[stub] {}", msg.as_ref());
    if let Ok(path) = std::env::var("STUB_LOG")
        && let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
    {
        let _ = writeln!(f, "{}", msg.as_ref());
    }
}

fn send(cx: &ConnectionTo<Client>, update: SessionUpdate) -> Result<()> {
    cx.send_notification(SessionNotification::new(SESSION_ID, update))
}

fn agent_text(cx: &ConnectionTo<Client>, text: impl Into<String>) -> Result<()> {
    send(
        cx,
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from(text.into()))),
    )
}

fn user_text(cx: &ConnectionTo<Client>, text: impl Into<String>) -> Result<()> {
    send(
        cx,
        SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::from(text.into()))),
    )
}

fn commands(cx: &ConnectionTo<Client>) -> Result<()> {
    let cmds = [
        ("queue", "Show the priority queue"),
        ("more", "Expand the last update one level"),
        ("less", "Collapse the last update"),
        (
            "notify",
            "Spike: send an out-of-turn update after N seconds",
        ),
        ("tool", "Spike: emit an inline tool call"),
        ("delegate", "Spike: emit a delegation tool call"),
    ];
    send(
        cx,
        SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(
            cmds.iter()
                .map(|(n, d)| AvailableCommand::new(*n, *d))
                .collect(),
        )),
    )
}

fn queue_plan(cx: &ConnectionTo<Client>, done_first: bool) -> Result<()> {
    let first = if done_first {
        PlanEntryStatus::Completed
    } else {
        PlanEntryStatus::InProgress
    };
    send(
        cx,
        SessionUpdate::Plan(Plan::new(vec![
            PlanEntry::new(
                "P0 · Fix red main build (repo-x)",
                PlanEntryPriority::High,
                first,
            ),
            PlanEntry::new(
                "P1 · Answer review request on PR 42",
                PlanEntryPriority::High,
                PlanEntryStatus::Pending,
            ),
            PlanEntry::new(
                "P2 · Refactor auth middleware",
                PlanEntryPriority::Medium,
                PlanEntryStatus::Pending,
            ),
            PlanEntry::new(
                "P3 · Bump tokio",
                PlanEntryPriority::Low,
                PlanEntryStatus::Pending,
            ),
        ])),
    )
}

fn replay(cx: &ConnectionTo<Client>, state: &State) -> Result<()> {
    let history = state.history.lock().unwrap().clone();
    log(format!("replaying {} turns", history.len()));
    for t in history {
        match t.role.as_str() {
            "user" => user_text(cx, t.text)?,
            _ => agent_text(cx, t.text)?,
        }
    }
    Ok(())
}

fn tool_call(cx: &ConnectionTo<Client>, state: &State, title: &str, result: &str) -> Result<()> {
    let id = state.next_tool_id();
    send(
        cx,
        SessionUpdate::ToolCall(
            ToolCall::new(id.clone(), title).status(ToolCallStatus::InProgress),
        ),
    )?;
    send(
        cx,
        SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            id,
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::Completed)
                .content(vec![ContentBlock::from(result.to_string()).into()]),
        )),
    )
}

async fn handle_prompt(cx: ConnectionTo<Client>, state: State, text: String) -> Result<StopReason> {
    state.push("user", &text);
    let trimmed = text.trim();
    let reply = if let Some(rest) = trimmed.strip_prefix("/notify") {
        let secs: u64 = rest.trim().parse().unwrap_or(5);
        let cx2 = cx.clone();
        let state2 = state.clone();
        cx.spawn(async move {
            tokio::time::sleep(Duration::from_secs(secs)).await;
            log("sending out-of-turn update");
            let msg = format!("⏰ Out-of-turn update ({secs}s later): worker T-7 finished. Main is green. /more for detail.");
            state2.push("agent", &msg);
            agent_text(&cx2, msg)?;
            queue_plan(&cx2, true)?;
            Ok(())
        })?;
        format!("Scheduled an out-of-turn update in {secs}s. The prompt turn ends now.")
    } else if trimmed.starts_with("/queue") {
        queue_plan(&cx, false)?;
        "Queue mirrored into the plan panel.".to_string()
    } else if trimmed.starts_with("/tool") {
        tool_call(
            &cx,
            &state,
            "ws_search \"flaky\" in repo-x",
            "3 matches in tests/auth.rs",
        )?;
        "Ran an inline lookup (fake).".to_string()
    } else if trimmed.starts_with("/delegate") {
        tool_call(
            &cx,
            &state,
            "Delegated → T-123: Fix flaky auth test (repo-x)",
            "Worker started on executor laptop",
        )?;
        "Handing this to a worker: it touches several files.".to_string()
    } else if trimmed.starts_with("/more") {
        "**L2 detail (fake):**\n- Plan: 3/4 steps done\n- Files: `src/auth.rs` (+12 −4)\n- Tests: 48 passed".to_string()
    } else {
        format!("You said: {trimmed}")
    };
    // Stream the reply in a few chunks to look like real output.
    for chunk in reply.split_inclusive(' ').collect::<Vec<_>>().chunks(4) {
        agent_text(&cx, chunk.concat())?;
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    state.push("agent", &reply);
    Ok(StopReason::EndTurn)
}

fn prompt_text(req: &PromptRequest) -> String {
    req.prompt
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::main]
async fn main() -> Result<()> {
    let path = std::env::var("STUB_HISTORY")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("console-stub-history.json"));
    let state = State::load(path);
    log(format!("started pid={}", std::process::id()));

    Agent
        .v1()
        .name("orch-stub")
        .on_receive_request(
            async move |req: InitializeRequest, responder, _cx| {
                log(format!(
                    "initialize client={:?}",
                    req.client_info.as_ref().map(|c| c.name.clone())
                ));
                responder.respond(
                    InitializeResponse::new(req.protocol_version)
                        .agent_capabilities(AgentCapabilities::new().load_session(true)),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                async move |req: NewSessionRequest, responder, cx: ConnectionTo<Client>| {
                    log(format!(
                        "session/new cwd={:?} mcp_servers={}",
                        req.cwd,
                        req.mcp_servers.len()
                    ));
                    // Probe: updates sent *before* the response (Toad doesn't know the id yet).
                    replay(&cx, &state)?;
                    responder.respond(NewSessionResponse::new(SESSION_ID))?;
                    // Probe: updates sent right *after* the response, outside any prompt.
                    let cx2 = cx.clone();
                    cx.spawn(async move {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        commands(&cx2)?;
                        agent_text(
                            &cx2,
                            "👋 Orchestrator stub ready (sent after session/new, outside a turn).",
                        )?;
                        queue_plan(&cx2, false)?;
                        Ok(())
                    })
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                async move |req: LoadSessionRequest, responder, cx: ConnectionTo<Client>| {
                    log(format!("session/load id={}", req.session_id));
                    replay(&cx, &state)?;
                    commands(&cx)?;
                    queue_plan(&cx, false)?;
                    responder.respond(LoadSessionResponse::new())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let state = state.clone();
                async move |req: PromptRequest, responder, cx: ConnectionTo<Client>| {
                    let text = prompt_text(&req);
                    log(format!("session/prompt {text:?}"));
                    let state = state.clone();
                    let cx2 = cx.clone();
                    cx.spawn(async move {
                        let stop = handle_prompt(cx2, state, text).await?;
                        responder.respond(PromptResponse::new(stop))
                    })
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |n: CancelNotification, _cx| {
                log(format!("session/cancel {}", n.session_id));
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await
}
