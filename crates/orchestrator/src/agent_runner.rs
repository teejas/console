//! Runs Orchestrator turns through the hosted Claude Code process, one at a time.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use console_cc_host::{CcConfig, CcEvent, CcProcess};
use console_proto::TurnId;
use console_proto::domain::DelegationDecision;
use console_proto::event::EventBody;
use console_store::{EventFilter, EventKeys, TurnEnd};
use tokio::sync::mpsc;

use crate::App;

/// Streamed text is coalesced into events at most this often.
const FLUSH_EVERY: Duration = Duration::from_millis(250);

#[derive(Debug)]
pub struct AgentRunner {
    tx: mpsc::UnboundedSender<TurnId>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<TurnId>>>,
    current: Mutex<Option<Current>>,
}

#[derive(Debug, Clone, Copy)]
struct Current {
    turn: TurnId,
    started: Instant,
    delegated: bool,
}

impl Default for AgentRunner {
    fn default() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            tx,
            rx: Mutex::new(Some(rx)),
            current: Mutex::new(None),
        }
    }
}

impl AgentRunner {
    /// Queues a stored turn for the agent.
    pub fn enqueue(&self, turn: TurnId) {
        let _ = self.tx.send(turn);
    }

    /// The turn in flight and when it started.
    #[must_use]
    pub fn current(&self) -> Option<(TurnId, Instant)> {
        self.current.lock().ok()?.map(|c| (c.turn, c.started))
    }

    /// Records that the current turn handed work to a worker.
    pub fn mark_delegated(&self) {
        if let Ok(mut c) = self.current.lock()
            && let Some(c) = c.as_mut()
        {
            c.delegated = true;
        }
    }

    /// Marks a turn as in flight without running it (tests of the budget hooks).
    #[doc(hidden)]
    pub fn begin_for_test(&self, turn: TurnId) {
        self.set_current(Some(Current {
            turn,
            started: Instant::now(),
            delegated: false,
        }));
    }

    fn set_current(&self, value: Option<Current>) {
        if let Ok(mut c) = self.current.lock() {
            *c = value;
        }
    }

    fn take_current(&self) -> Option<Current> {
        self.current.lock().ok().and_then(|mut c| c.take())
    }
}

/// The process configuration for the Orchestrator agent.
pub fn cc_config(app: &App, resume: Option<String>) -> std::io::Result<CcConfig> {
    let dir = app.config.data_dir.join("agent");
    std::fs::create_dir_all(&dir)?;
    let exe = std::env::current_exe()?;
    let settings = dir.join("settings.json");
    let mcp = dir.join("mcp.json");
    let url = format!("http://{}/mcp", app.config.orchestrator.listen);
    std::fs::write(
        &settings,
        console_agent::settings(&exe.to_string_lossy()).to_string(),
    )?;
    std::fs::write(
        &mcp,
        console_agent::mcp_config(&url, &app.token).to_string(),
    )?;
    let agent = &app.config.agent;
    let args = vec![
        "--model".into(),
        agent.model.clone(),
        "--effort".into(),
        agent.effort.as_flag().into(),
        "--tools".into(),
        String::new(),
        "--strict-mcp-config".into(),
        "--mcp-config".into(),
        mcp.to_string_lossy().into_owned(),
        "--setting-sources".into(),
        String::new(),
        "--settings".into(),
        settings.to_string_lossy().into_owned(),
        "--system-prompt".into(),
        console_agent::SYSTEM_PROMPT.into(),
        "--permission-prompts".into(),
        "none".into(),
    ];
    Ok(CcConfig {
        bin: agent.claude_bin.clone(),
        args,
        cwd: dir.join("home"),
        env: vec![
            ("CONSOLE_HOOK_URL".into(), app.config.client_url()),
            ("CONSOLE_HOOK_TOKEN".into(), app.token.clone()),
        ],
        resume,
    })
}

/// Starts the runner loop. Call once.
pub fn start(app: &Arc<App>) {
    let Some(rx) = app.agent.rx.lock().ok().and_then(|mut r| r.take()) else {
        return;
    };
    tokio::spawn(run(app.clone(), rx));
}

async fn run(app: Arc<App>, mut rx: mpsc::UnboundedReceiver<TurnId>) {
    // Turns that were running when the service stopped can't be resumed mid-stream.
    for t in app.store.unfinished_turns().await.unwrap_or_default() {
        if t.status == "running" {
            let _ = app
                .store
                .write(async |tx| {
                    tx.append(
                        EventKeys::session(t.session_id),
                        EventBody::AgentText {
                            turn_id: Some(t.id),
                            text: "(This turn was interrupted by a restart. Ask again if you still need it.)".into(),
                        },
                    )
                    .await?;
                    tx.end_turn(t.id, failed_end(0)).await
                })
                .await;
        } else {
            app.agent.enqueue(t.id);
        }
    }

    let mut proc: Option<CcProcess> = None;
    let mut seed: Option<String> = None;
    while let Some(turn_id) = rx.recv().await {
        let Ok(Some(turn)) = app.store.turn(turn_id).await else {
            continue;
        };
        if turn.status != "pending" {
            continue;
        }
        let _ = app
            .store
            .write(async |tx| tx.set_turn_status(turn_id, "running").await)
            .await;
        app.agent.set_current(Some(Current {
            turn: turn_id,
            started: Instant::now(),
            delegated: false,
        }));
        let mut attempts = 0;
        loop {
            attempts += 1;
            let resumed;
            if proc.is_none() {
                let resume = app
                    .store
                    .session(app.orchestrator_session)
                    .await
                    .ok()
                    .flatten()
                    .and_then(|s| s.harness_session_ref)
                    .filter(|r| !r.is_empty());
                resumed = resume.is_some();
                match cc_config(&app, resume) {
                    Ok(cfg) => match CcProcess::spawn(&cfg).await {
                        Ok(p) => proc = Some(p),
                        Err(e) => {
                            finish_failed(
                                &app,
                                turn_id,
                                &format!(
                                    "Couldn't start the Orchestrator agent ({}): {e}",
                                    app.config.agent.claude_bin.display()
                                ),
                            )
                            .await;
                            break;
                        }
                    },
                    Err(e) => {
                        finish_failed(
                            &app,
                            turn_id,
                            &format!("Couldn't prepare the Orchestrator agent: {e}"),
                        )
                        .await;
                        break;
                    }
                }
            } else {
                resumed = false;
            }
            let Some(p) = proc.as_mut() else { break };
            let text = match seed.take() {
                Some(s) => format!("{s}\n\n{}", turn.user_message),
                None => turn.user_message.clone(),
            };
            match drive(&app, p, turn_id, &text).await {
                Outcome::Done => break,
                Outcome::Exited { early, stderr } => {
                    proc = None;
                    if early && resumed && attempts < 3 {
                        // Resume failed: forget the session and start fresh with context.
                        tracing::warn!(%stderr, "resuming the agent session failed; starting a new one");
                        clear_session_ref(&app).await;
                        seed = Some(conversation_summary(&app).await);
                        continue;
                    }
                    let msg = format!(
                        "The Orchestrator agent stopped unexpectedly. {}",
                        stderr.lines().last().unwrap_or("")
                    );
                    finish_failed(&app, turn_id, msg.trim()).await;
                    break;
                }
            }
        }
        app.agent.set_current(None);
    }
}

async fn clear_session_ref(app: &App) {
    let sid = app.orchestrator_session;
    let _ = app
        .store
        .write(async |tx| tx.set_harness_session_ref(sid, "").await)
        .await;
}

fn failed_end(wall_ms: u64) -> TurnEnd {
    TurnEnd {
        ok: false,
        decision: DelegationDecision::Inline,
        reason: None,
        wall_ms,
        budget_hit: false,
        cost_usd: None,
    }
}

async fn finish_failed(app: &App, turn: TurnId, message: &str) {
    let wall = app
        .agent
        .take_current()
        .map_or(0, |c| c.started.elapsed().as_millis());
    let sid = app.orchestrator_session;
    let _ = app
        .store
        .write(async |tx| {
            tx.append(
                EventKeys::session(sid),
                EventBody::AgentText {
                    turn_id: Some(turn),
                    text: format!("⚠ {message}"),
                },
            )
            .await?;
            tx.end_turn(turn, failed_end(u64::try_from(wall).unwrap_or(u64::MAX)))
                .await
        })
        .await;
}

/// A short recap of recent conversation, used to seed a fresh session.
async fn conversation_summary(app: &App) -> String {
    let events = app
        .store
        .recent_events(&EventFilter::Conversation(app.orchestrator_session), 60)
        .await
        .unwrap_or_default();
    let mut lines = Vec::new();
    for e in events {
        match e.body {
            EventBody::UserMessage { text, .. } => lines.push(format!(
                "User: {}",
                text.chars().take(400).collect::<String>()
            )),
            EventBody::AgentText { text, .. } => match lines.last_mut() {
                Some(last) if last.starts_with("You:") => last.push_str(&text),
                _ => lines.push(format!("You: {text}")),
            },
            _ => {}
        }
    }
    let recap: Vec<String> = lines
        .into_iter()
        .rev()
        .take(20)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!(
        "(Context: your previous session couldn't be resumed. Recent conversation, oldest first:)\n{}\n(End of context.)",
        recap.join("\n")
    )
}

enum Outcome {
    Done,
    /// The process exited. `early` means before it produced anything for this turn.
    Exited {
        early: bool,
        stderr: String,
    },
}

fn title_for(name: &str, input: &serde_json::Value) -> String {
    let short = name
        .strip_prefix(&format!("mcp__{}__", console_agent::MCP_SERVER))
        .unwrap_or(name);
    let args = input
        .as_object()
        .map(|o| {
            o.iter()
                .filter(|(k, _)| {
                    !matches!(
                        k.as_str(),
                        "context"
                            | "goal"
                            | "constraints"
                            | "acceptance_criteria"
                            | "suggested_checks"
                    )
                })
                .map(|(k, v)| match v {
                    serde_json::Value::String(s) => {
                        format!("{k}={}", s.chars().take(60).collect::<String>())
                    }
                    other => format!("{k}={other}"),
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    let t = format!("{short} {args}");
    t.trim().chars().take(140).collect()
}

/// Sends one prompt and processes events until the turn's result.
async fn drive(app: &App, p: &mut CcProcess, turn: TurnId, text: &str) -> Outcome {
    let sid = app.orchestrator_session;
    if let Err(e) = p.send_user(text).await {
        return Outcome::Exited {
            early: true,
            stderr: e.to_string(),
        };
    }
    let mut buf = String::new();
    let mut last_flush = Instant::now();
    let mut titles: HashMap<String, String> = HashMap::new();
    let mut saw_output = false;
    let flush = async |buf: &mut String| {
        if buf.is_empty() {
            return;
        }
        let text = std::mem::take(buf);
        let _ = app
            .store
            .write(async |tx| {
                tx.append(
                    EventKeys::session(sid),
                    EventBody::AgentText {
                        turn_id: Some(turn),
                        text,
                    },
                )
                .await
            })
            .await;
    };
    while let Some(ev) = p.next().await {
        match ev {
            CcEvent::Init { session_id } => {
                let _ = app
                    .store
                    .write(async |tx| tx.set_harness_session_ref(sid, &session_id).await)
                    .await;
            }
            CcEvent::TextDelta(t) => {
                saw_output = true;
                buf.push_str(&t);
                if last_flush.elapsed() >= FLUSH_EVERY {
                    flush(&mut buf).await;
                    last_flush = Instant::now();
                }
            }
            CcEvent::ToolUse { id, name, input } => {
                saw_output = true;
                flush(&mut buf).await;
                let title = title_for(&name, &input);
                titles.insert(id.clone(), title.clone());
                let _ = app
                    .store
                    .write(async |tx| {
                        tx.append(
                            EventKeys::session(sid),
                            EventBody::ToolCallStarted {
                                turn_id: Some(turn),
                                tool_call_id: id,
                                title,
                            },
                        )
                        .await
                    })
                    .await;
            }
            CcEvent::ToolResult { id, is_error, text } => {
                flush(&mut buf).await;
                let summary = Some(
                    text.lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(200)
                        .collect::<String>(),
                )
                .filter(|s| !s.is_empty());
                let _ = app
                    .store
                    .write(async |tx| {
                        tx.append(
                            EventKeys::session(sid),
                            EventBody::ToolCallFinished {
                                tool_call_id: id,
                                ok: !is_error,
                                summary,
                            },
                        )
                        .await
                    })
                    .await;
            }
            CcEvent::Result {
                session_id,
                is_error,
                cost_usd,
                text,
            } => {
                flush(&mut buf).await;
                let current = app.agent.take_current();
                let wall = current.map_or(0, |c| c.started.elapsed().as_millis());
                let decision = if current.is_some_and(|c| c.delegated) {
                    DelegationDecision::Delegate
                } else {
                    DelegationDecision::Inline
                };
                if is_error && let Some(t) = text.filter(|t| !t.is_empty()) {
                    let _ = app
                        .store
                        .write(async |tx| {
                            tx.append(
                                EventKeys::session(sid),
                                EventBody::AgentText {
                                    turn_id: Some(turn),
                                    text: format!("⚠ {t}"),
                                },
                            )
                            .await
                        })
                        .await;
                }
                if let Some(s) = session_id {
                    let _ = app
                        .store
                        .write(async |tx| tx.set_harness_session_ref(sid, &s).await)
                        .await;
                }
                let end = TurnEnd {
                    ok: !is_error,
                    decision,
                    reason: None,
                    wall_ms: u64::try_from(wall).unwrap_or(u64::MAX),
                    budget_hit: false,
                    cost_usd,
                };
                let _ = app
                    .store
                    .write(async |tx| tx.end_turn(turn, end).await)
                    .await;
                return Outcome::Done;
            }
            CcEvent::Exited { stderr, .. } => {
                flush(&mut buf).await;
                return Outcome::Exited {
                    early: !saw_output,
                    stderr,
                };
            }
        }
    }
    Outcome::Exited {
        early: !saw_output,
        stderr: String::new(),
    }
}

/// Where the agent's files live (for `console inspect` and docs).
#[must_use]
pub fn agent_dir(app: &App) -> PathBuf {
    app.config.data_dir.join("agent")
}
