//! `console acp`: an ACP v1 agent on stdio that bridges Toad to the Orchestrator client API (M6).
//!
//! See `docs/specs/clients.md`. The default session is the Orchestrator conversation;
//! `--task` attaches to a worker's session instead. Slash commands are handled here
//! through the client API; everything else is a turn.

pub mod render;

use std::collections::HashSet;
use std::str::FromStr as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    AgentCapabilities, AvailableCommand, AvailableCommandsUpdate, CancelNotification, ContentBlock,
    Implementation, InitializeRequest, InitializeResponse, LoadSessionRequest, LoadSessionResponse,
    NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, SessionNotification,
    SessionUpdate, StopReason,
};
use agent_client_protocol::{Agent, Client as AcpClient, Stdio};
use console_client::Client;
use console_proto::api::{
    AttachParams, AttachResult, DecisionRespond, DecisionRespondParams, Empty, EventsSubscribe,
    ProposalList, ProposalResolve, ProposalResolveParams, QueueList, QueueListParams,
    SessionAttach, SessionTarget, SubscribeParams, TaskCancel, TaskCancelParams, TaskGetParams,
    TaskList, TaskListParams, TaskPatch, TodoAdd, TodoAddParams, TodoUpdate, TodoUpdateParams,
    TurnOverrideMethod, TurnOverrideParams, TurnSubmit, TurnSubmitParams, WorkerPrompt,
    WorkerPromptParams,
};
use console_proto::domain::{TodoStatus, TurnOverride};
use console_proto::event::EventBody;
use console_proto::rpc::EventNotification;
use console_proto::{TaskId, TurnId};
use tokio::sync::{mpsc, oneshot};

use crate::render::{Renderer, plan, text};

const COMMANDS: &[(&str, &str)] = &[
    ("queue", "Show the shared priority queue"),
    ("next", "What to work on next"),
    ("why", "Why an item sits where it does: /why <n>"),
    ("add", "Add a todo: /add <title>"),
    ("done", "Mark a todo done: /done <n>"),
    ("proposals", "The Orchestrator's open proposals"),
    ("accept", "Accept a proposal: /accept <n>"),
    ("reject", "Reject a proposal: /reject <n>"),
    ("tasks", "Active tasks"),
    ("decisions", "Questions and permissions waiting on you"),
    ("answer", "Answer a decision: /answer [n] <option or text>"),
    ("diff", "A task's changes: /diff <task>"),
    ("cancel", "Cancel a task: /cancel <task>"),
    ("attach", "Open a worker's own session: /attach <task>"),
    ("delegate", "Hand your last request to a worker"),
    ("inline", "Answer your last request here instead"),
];

struct Bridge {
    client: Client,
    attached: AttachResult,
    worker_task: Option<TaskId>,
    renderer: Mutex<Renderer>,
    cx: Mutex<Option<ConnectionTo<AcpClient>>>,
    waiting: Mutex<std::collections::HashMap<TurnId, oneshot::Sender<()>>>,
    ended: Mutex<HashSet<TurnId>>,
    last_turn: Mutex<Option<TurnId>>,
    events: Mutex<Option<mpsc::UnboundedReceiver<EventNotification>>>,
    /// The last plan sent; Toad also draws plans inline, so unchanged plans aren't resent.
    last_plan: Mutex<Vec<String>>,
}

impl std::fmt::Debug for Bridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bridge")
            .field("session", &self.attached.session_id)
            .finish_non_exhaustive()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Bridge {
    fn acp_session(&self) -> String {
        self.attached.session_id.to_string()
    }

    fn send(&self, updates: Vec<SessionUpdate>) {
        let Some(cx) = lock(&self.cx).clone() else {
            return;
        };
        for u in updates {
            let _ = cx.send_notification(SessionNotification::new(self.acp_session(), u));
        }
    }

    fn say(&self, msg: impl Into<String>) {
        self.send(vec![text(msg)]);
    }

    async fn refresh_plan(&self) {
        if self.worker_task.is_some() {
            return;
        }
        if let Ok(q) = self
            .client
            .call::<QueueList>(QueueListParams::default())
            .await
        {
            let top: Vec<_> = q.into_iter().take(8).collect();
            let key: Vec<String> = top
                .iter()
                .map(|t| format!("{:?}{:?}{}", t.band, t.status, t.title))
                .collect();
            {
                let mut last = lock(&self.last_plan);
                if *last == key {
                    return;
                }
                *last = key;
            }
            self.send(vec![plan(&top)]);
        }
    }

    fn replay(&self) {
        let mut r = lock(&self.renderer);
        let mut updates = Vec::new();
        for e in &self.attached.recent_events {
            updates.extend(r.render(e, true));
        }
        drop(r);
        self.send(updates);
    }

    async fn pump(self: Arc<Self>) {
        let Some(mut rx) = lock(&self.events).take() else {
            return;
        };
        let mut queue_dirty = false;
        loop {
            let next = if queue_dirty {
                match tokio::time::timeout(Duration::from_millis(300), rx.recv()).await {
                    Ok(n) => n,
                    Err(_) => {
                        queue_dirty = false;
                        self.refresh_plan().await;
                        continue;
                    }
                }
            } else {
                rx.recv().await
            };
            let Some(note) = next else { return };
            let e = note.event;
            if matches!(
                e.body,
                EventBody::TodoChanged { .. } | EventBody::TaskTransition { .. }
            ) {
                queue_dirty = true;
            }
            if let EventBody::TurnEnded { turn_id, .. } = e.body {
                lock(&self.ended).insert(turn_id);
                if let Some(tx) = lock(&self.waiting).remove(&turn_id) {
                    let _ = tx.send(());
                }
            }
            let updates = lock(&self.renderer).render(&e, false);
            self.send(updates);
        }
    }

    async fn wait_for_turn(&self, turn: TurnId) {
        let (tx, rx) = oneshot::channel();
        lock(&self.waiting).insert(turn, tx);
        if lock(&self.ended).contains(&turn) {
            lock(&self.waiting).remove(&turn);
            return;
        }
        let _ = rx.await;
    }

    async fn prompt(self: Arc<Self>, input: String) -> StopReason {
        let trimmed = input.trim();
        if let Some(cmd) = trimmed.strip_prefix('/') {
            let reply = match self.command(cmd).await {
                Ok(s) | Err(s) => s,
            };
            self.say(reply);
            return StopReason::EndTurn;
        }
        if let Some(task_id) = self.worker_task {
            let key = format!("acp-{}", TurnId::generate());
            match self
                .client
                .call::<WorkerPrompt>(WorkerPromptParams {
                    task_id,
                    text: input,
                    idempotency_key: key,
                })
                .await
            {
                Ok(_) => {}
                Err(e) => self.say(format!("Couldn't reach the worker: {e}")),
            }
            return StopReason::EndTurn;
        }
        let key = format!("acp-{}", TurnId::generate());
        match self
            .client
            .call::<TurnSubmit>(TurnSubmitParams {
                text: input,
                idempotency_key: key,
            })
            .await
        {
            Ok(r) => {
                lock(&self.renderer).begin_turn(r.turn_id);
                *lock(&self.last_turn) = Some(r.turn_id);
                self.wait_for_turn(r.turn_id).await;
                StopReason::EndTurn
            }
            Err(e) => {
                self.say(format!("⚠ Couldn't reach the Orchestrator: {e}"));
                StopReason::EndTurn
            }
        }
    }

    async fn find_task(&self, reference: &str) -> Result<TaskId, String> {
        let tasks = self
            .client
            .call::<TaskList>(TaskListParams { include_done: true })
            .await
            .map_err(|e| e.to_string())?;
        let active: Vec<_> = tasks.iter().filter(|t| !t.state.is_terminal()).collect();
        if let Ok(n) = reference.parse::<usize>()
            && n >= 1
            && n <= active.len()
        {
            return Ok(active[n - 1].id);
        }
        let needle = reference.to_lowercase();
        let m: Vec<_> = tasks
            .iter()
            .filter(|t| t.id.to_string().to_lowercase().ends_with(&needle))
            .collect();
        match m.as_slice() {
            [t] => Ok(t.id),
            [] => Err(format!("No task matches {reference:?}. See /tasks.")),
            _ => Err(format!(
                "{reference:?} matches several tasks; use more of the ID."
            )),
        }
    }

    async fn command(&self, line: &str) -> Result<String, String> {
        let (name, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim();
        let e = |e: console_client::ClientError| e.to_string();
        match name {
            "queue" | "next" | "why" => {
                let q = self
                    .client
                    .call::<QueueList>(QueueListParams::default())
                    .await
                    .map_err(e)?;
                self.refresh_plan().await;
                if q.is_empty() {
                    return Ok("The queue is empty. Add something with /add.".into());
                }
                if name == "next" {
                    let t = &q[0];
                    return Ok(format!("Next: **{}** ({:?}). {}", t.title, t.band, t.why));
                }
                if name == "why" {
                    let n: usize = rest.parse().map_err(|_| "Usage: /why <n>".to_owned())?;
                    let t = q.get(n.saturating_sub(1)).ok_or("No such item.")?;
                    return Ok(format!("**{}**: {}", t.title, t.why));
                }
                Ok(q.iter()
                    .enumerate()
                    .map(|(i, t)| {
                        format!(
                            "{}. {:?} {}{}",
                            i + 1,
                            t.band,
                            t.title,
                            if t.status == TodoStatus::WaitingOnUser {
                                " (needs you)"
                            } else {
                                ""
                            }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            "add" => {
                if rest.is_empty() {
                    return Err("Usage: /add <title>".into());
                }
                let t = self
                    .client
                    .call::<TodoAdd>(TodoAddParams {
                        title: rest.into(),
                        body: None,
                        kind: None,
                        importance: None,
                        urgency: None,
                        due_at: None,
                        workspace_id: None,
                    })
                    .await
                    .map_err(e)?;
                Ok(format!(
                    "Added \"{}\" ({:?}). Ask me to set its priority, or edit it with `console todo edit`.",
                    t.title, t.band
                ))
            }
            "done" => {
                let q = self
                    .client
                    .call::<QueueList>(QueueListParams::default())
                    .await
                    .map_err(e)?;
                let n: usize = rest.parse().map_err(|_| "Usage: /done <n>".to_owned())?;
                let t = q.get(n.saturating_sub(1)).ok_or("No such item.")?;
                self.client
                    .call::<TodoUpdate>(TodoUpdateParams {
                        todo_id: t.id,
                        expected_version: t.version,
                        title: None,
                        kind: None,
                        owner: None,
                        status: Some(TodoStatus::Done),
                        importance: None,
                        urgency: None,
                        due_at: None,
                        clear_due: false,
                        workspace_id: None,
                    })
                    .await
                    .map_err(e)?;
                Ok(format!("Done: \"{}\".", t.title))
            }
            "proposals" => {
                let ps = self
                    .client
                    .call::<ProposalList>(Empty {})
                    .await
                    .map_err(e)?;
                if ps.is_empty() {
                    return Ok("No open proposals.".into());
                }
                Ok(ps
                    .iter()
                    .enumerate()
                    .map(|(i, p)| {
                        format!(
                            "{}. \"{}\": set {} to {} ({})",
                            i + 1,
                            p.todo_title,
                            p.field,
                            p.value,
                            p.rationale
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            "accept" | "reject" => {
                let ps = self
                    .client
                    .call::<ProposalList>(Empty {})
                    .await
                    .map_err(e)?;
                let n: usize = rest.parse().map_err(|_| format!("Usage: /{name} <n>"))?;
                let p = ps
                    .get(n.saturating_sub(1))
                    .ok_or("No such proposal; see /proposals.")?;
                let t = self
                    .client
                    .call::<ProposalResolve>(ProposalResolveParams {
                        proposal_id: p.id,
                        accept: name == "accept",
                    })
                    .await
                    .map_err(e)?;
                Ok(format!(
                    "{} for \"{}\".",
                    if name == "accept" {
                        "Accepted"
                    } else {
                        "Rejected"
                    },
                    t.title
                ))
            }
            "tasks" => {
                let ts = self
                    .client
                    .call::<TaskList>(TaskListParams::default())
                    .await
                    .map_err(e)?;
                if ts.is_empty() {
                    return Ok("No active tasks.".into());
                }
                Ok(ts
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        format!(
                            "{}. {} — {:?}{} [{}]",
                            i + 1,
                            t.title,
                            t.state,
                            t.blocked.map(|b| format!(" ({b:?})")).unwrap_or_default(),
                            short(&t.id)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            "decisions" => {
                let ds = self.open_decisions().await?;
                if ds.is_empty() {
                    return Ok("Nothing is waiting on you.".into());
                }
                Ok(ds
                    .iter()
                    .enumerate()
                    .map(|(i, d)| {
                        let opts: Vec<&str> = d.options.iter().map(|o| o.id.as_str()).collect();
                        format!(
                            "{}. {} {}",
                            i + 1,
                            d.question,
                            if opts.is_empty() {
                                "(free text)".into()
                            } else {
                                format!("[{}]", opts.join(" / "))
                            }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            "answer" => {
                let ds = self.open_decisions().await?;
                if ds.is_empty() {
                    return Err("Nothing is waiting on you.".into());
                }
                let (index, answer) = match rest.split_once(char::is_whitespace) {
                    Some((n, a)) if n.parse::<usize>().is_ok() => {
                        (n.parse::<usize>().unwrap_or(1), a.trim())
                    }
                    _ => (1, rest),
                };
                if answer.is_empty() {
                    return Err("Usage: /answer [n] <option or text>".into());
                }
                let d = ds
                    .get(index.saturating_sub(1))
                    .ok_or("No such decision; see /decisions.")?;
                let (option_id, text) = if d.options.is_empty() {
                    ("answer".to_owned(), Some(answer.to_owned()))
                } else {
                    let o = d
                        .options
                        .iter()
                        .find(|o| {
                            o.id.eq_ignore_ascii_case(answer)
                                || o.label.eq_ignore_ascii_case(answer)
                        })
                        .ok_or_else(|| {
                            format!(
                                "Choose one of: {}",
                                d.options
                                    .iter()
                                    .map(|o| o.id.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )
                        })?;
                    (o.id.clone(), None)
                };
                self.client
                    .call::<DecisionRespond>(DecisionRespondParams {
                        decision_id: d.id,
                        option_id,
                        expected_state: None,
                        answer: text,
                    })
                    .await
                    .map_err(e)?;
                Ok(format!("Sent: {answer}"))
            }
            "diff" => {
                let id = match (rest.is_empty(), self.worker_task) {
                    (true, Some(t)) => t,
                    (true, None) => return Err("Usage: /diff <task>".into()),
                    _ => self.find_task(rest).await?,
                };
                let p = self
                    .client
                    .call::<TaskPatch>(TaskGetParams { task_id: id })
                    .await
                    .map_err(e)?;
                Ok(match p.patch {
                    Some(patch) if !patch.is_empty() => format!(
                        "```diff\n{}\n```",
                        patch.chars().take(60_000).collect::<String>()
                    ),
                    _ => "No changes captured for that task yet.".into(),
                })
            }
            "cancel" => {
                let id = match (rest.is_empty(), self.worker_task) {
                    (true, Some(t)) => t,
                    (true, None) => return Err("Usage: /cancel <task>".into()),
                    _ => self.find_task(rest).await?,
                };
                let r = self
                    .client
                    .call::<TaskCancel>(TaskCancelParams { task_id: id })
                    .await
                    .map_err(e)?;
                Ok(format!("Cancel requested ({:?}).", r.state))
            }
            "attach" => {
                let id = self.find_task(rest).await?;
                Ok(format!(
                    "To watch or steer this worker, open another Toad session with the agent command:\n\n`console acp --task {id}`"
                ))
            }
            "delegate" | "inline" => {
                let turn = (*lock(&self.last_turn))
                    .ok_or("There's no earlier request in this session.")?;
                let mode = if name == "delegate" {
                    TurnOverride::Delegate
                } else {
                    TurnOverride::Inline
                };
                self.client
                    .call::<TurnOverrideMethod>(TurnOverrideParams {
                        turn_id: turn,
                        mode,
                    })
                    .await
                    .map_err(e)?;
                Ok(if name == "delegate" {
                    "Handing it to a worker…".into()
                } else {
                    "Answering here instead…".into()
                })
            }
            other => Err(format!(
                "Unknown command /{other}. Commands: {}",
                COMMANDS
                    .iter()
                    .map(|(n, _)| format!("/{n}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            )),
        }
    }

    async fn open_decisions(&self) -> Result<Vec<console_proto::api::DecisionView>, String> {
        let a = self
            .client
            .call::<SessionAttach>(AttachParams {
                target: SessionTarget::Orchestrator,
                recent: 0,
            })
            .await
            .map_err(|e| e.to_string())?;
        Ok(match self.worker_task {
            Some(t) => a
                .open_decisions
                .into_iter()
                .filter(|d| d.task_id == t)
                .collect(),
            None => a.open_decisions,
        })
    }
}

fn short(id: &impl std::fmt::Display) -> String {
    let s = id.to_string();
    s[s.len().saturating_sub(6)..].to_lowercase()
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

fn session_opened(
    bridge: &Arc<Bridge>,
    cx: &ConnectionTo<AcpClient>,
) -> agent_client_protocol::Result<()> {
    *lock(&bridge.cx) = Some(cx.clone());
    bridge.replay();
    let b = bridge.clone();
    cx.spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        b.send(vec![SessionUpdate::AvailableCommandsUpdate(
            AvailableCommandsUpdate::new(
                COMMANDS
                    .iter()
                    .map(|(n, d)| AvailableCommand::new(*n, *d))
                    .collect(),
            ),
        )]);
        b.refresh_plan().await;
        b.pump().await;
        Ok(())
    })
}

/// Runs the bridge on stdin/stdout until the client disconnects.
pub async fn run(url: &str, token: &str, task: Option<String>) -> anyhow::Result<()> {
    let (client, events) = Client::connect(url, token).await?;
    let worker_task = task
        .as_deref()
        .map(TaskId::from_str)
        .transpose()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let target = match worker_task {
        Some(task_id) => SessionTarget::Task { task_id },
        None => SessionTarget::Orchestrator,
    };
    let attached = client
        .call::<SessionAttach>(AttachParams {
            target,
            recent: 200,
        })
        .await?;
    client
        .call::<EventsSubscribe>(SubscribeParams {
            session_id: attached.session_id,
            after: attached.cursor,
        })
        .await?;
    let bridge = Arc::new(Bridge {
        client,
        attached,
        worker_task,
        renderer: Mutex::new(Renderer::default()),
        cx: Mutex::new(None),
        waiting: Mutex::new(std::collections::HashMap::new()),
        ended: Mutex::new(HashSet::new()),
        last_turn: Mutex::new(None),
        events: Mutex::new(Some(events)),
        last_plan: Mutex::new(Vec::new()),
    });
    // Task titles for notices, from current tasks.
    if let Ok(ts) = bridge
        .client
        .call::<TaskList>(TaskListParams { include_done: true })
        .await
    {
        let mut r = lock(&bridge.renderer);
        for t in ts {
            r.titles.insert(t.id, t.title);
        }
    }

    let b_new = bridge.clone();
    let b_load = bridge.clone();
    let b_prompt = bridge.clone();
    let title = if worker_task.is_some() {
        "console worker"
    } else {
        "console"
    };
    Agent
        .v1()
        .name(title)
        .on_receive_request(
            async move |req: InitializeRequest, responder, _cx| {
                responder.respond(
                    InitializeResponse::new(req.protocol_version)
                        .agent_capabilities(AgentCapabilities::new().load_session(true))
                        .agent_info(Implementation::new("console", env!("CARGO_PKG_VERSION"))),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_req: NewSessionRequest, responder, cx: ConnectionTo<AcpClient>| {
                let sid = b_new.acp_session();
                responder.respond(NewSessionResponse::new(sid))?;
                session_opened(&b_new, &cx)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_req: LoadSessionRequest, responder, cx: ConnectionTo<AcpClient>| {
                session_opened(&b_load, &cx)?;
                responder.respond(LoadSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: PromptRequest, responder, cx: ConnectionTo<AcpClient>| {
                let b = b_prompt.clone();
                if lock(&b.cx).is_none() {
                    *lock(&b.cx) = Some(cx.clone());
                }
                let text = prompt_text(&req);
                cx.spawn(async move {
                    let stop = b.prompt(text).await;
                    responder.respond(PromptResponse::new(stop))
                })
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            async move |_n: CancelNotification, _cx| Ok(()),
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(Stdio::new())
        .await?;
    Ok(())
}
