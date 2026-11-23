//! Hosting workers: one ACP session per task, with progress, questions, permission
//! requests, completion, and patch capture.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use console_acp_host::{Control, McpHttp, PermissionAsk, WorkerConfig, WorkerEvent};
use console_proto::event::DecisionOption;
use console_proto::link::{CommandOutcome, ExecutorMsg, WorkerUpdate};
use console_proto::{CommandId, DecisionId, TaskId};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use crate::Executor;
use crate::workspaces::run_capture;

/// Streamed worker text is sent at most this often.
const FLUSH_EVERY: Duration = Duration::from_millis(400);

/// A running worker's control surface.
#[derive(Debug)]
pub struct WorkerCtl {
    pub task_id: TaskId,
    pub command_id: CommandId,
    control: mpsc::UnboundedSender<Control>,
    permissions: Mutex<HashMap<DecisionId, oneshot::Sender<Option<String>>>>,
    /// The open free-text question, if the worker is waiting for an answer.
    question: Mutex<Option<DecisionId>>,
    /// Set by the `complete` tool: summary and evidence.
    completed: Mutex<Option<String>>,
    last_text: Mutex<String>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl WorkerCtl {
    pub fn prompt(&self, text: String) -> bool {
        self.control.send(Control::Prompt(text)).is_ok()
    }

    pub fn complete(&self, summary: String) {
        *lock(&self.completed) = Some(summary);
    }

    pub fn is_completed(&self) -> bool {
        lock(&self.completed).is_some()
    }

    /// Opens a free-text question to the user on the worker's behalf.
    pub fn ask(&self, exec: &Executor, question: String) -> DecisionId {
        let did = DecisionId::generate();
        *lock(&self.question) = Some(did);
        exec.send(&ExecutorMsg::DecisionRequest {
            command_id: self.command_id,
            decision_id: did,
            question,
            options: Vec::new(),
        });
        did
    }

    /// Routes the user's answer. Returns false if this worker wasn't waiting on it.
    pub fn answer(&self, did: DecisionId, option_id: &str, answer: Option<String>) -> bool {
        if let Some(tx) = lock(&self.permissions).remove(&did) {
            let _ = tx.send(Some(option_id.to_owned()));
            return true;
        }
        let mut q = lock(&self.question);
        if *q == Some(did) {
            *q = None;
            let text = answer.unwrap_or_else(|| option_id.to_owned());
            let _ = self
                .control
                .send(Control::Prompt(format!("The user answered: {text}")));
            return true;
        }
        false
    }
}

#[derive(Debug, Default)]
pub struct Workers {
    map: Mutex<HashMap<TaskId, Arc<WorkerCtl>>>,
}

impl Workers {
    pub fn get(&self, task: TaskId) -> Option<Arc<WorkerCtl>> {
        lock(&self.map).get(&task).cloned()
    }

    pub fn all(&self) -> Vec<Arc<WorkerCtl>> {
        lock(&self.map).values().cloned().collect()
    }

    fn insert(&self, ctl: Arc<WorkerCtl>) {
        lock(&self.map).insert(ctl.task_id, ctl);
    }

    fn remove(&self, task: TaskId) {
        lock(&self.map).remove(&task);
    }
}

fn progress(exec: &Executor, cid: CommandId, update: WorkerUpdate) {
    exec.send(&ExecutorMsg::Progress {
        command_id: cid,
        update,
    });
}

/// Runs a worker for a task to completion and returns the command's outcome.
pub async fn run(
    exec: &Arc<Executor>,
    cid: CommandId,
    task_id: TaskId,
    root: &std::path::Path,
    harness: &str,
    brief: String,
    cancel: CancellationToken,
) -> CommandOutcome {
    let Some(command) = exec.config.executor.harnesses.get(harness).cloned() else {
        return CommandOutcome::Failed {
            error: format!("no harness {harness:?} configured on this executor"),
        };
    };
    let base = run_capture(root, "git", &["rev-parse", "HEAD"], Duration::from_secs(10))
        .await
        .ok()
        .filter(|c| c.ok)
        .map(|c| c.stdout.trim().to_owned());

    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
    let (ask_tx, mut ask_rx) = mpsc::unbounded_channel::<PermissionAsk>();
    let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
    let ctl = Arc::new(WorkerCtl {
        task_id,
        command_id: cid,
        control: ctl_tx.clone(),
        permissions: Mutex::new(HashMap::new()),
        question: Mutex::new(None),
        completed: Mutex::new(None),
        last_text: Mutex::new(String::new()),
    });
    exec.workers.insert(ctl.clone());

    let token = exec.config.token().unwrap_or_default();
    let mcp = exec.mcp_url().map(|url| McpHttp {
        name: "console".into(),
        url,
        headers: vec![
            ("Authorization".into(), format!("Bearer {token}")),
            (crate::worker_mcp::TASK_HEADER.into(), task_id.to_string()),
        ],
    });
    let cfg = WorkerConfig {
        command,
        env: Vec::new(),
        cwd: root.to_path_buf(),
        mcp,
    };
    let mut run = std::pin::pin!(console_acp_host::run(cfg, brief, ev_tx, ask_tx, ctl_rx));

    let mut buf = String::new();
    let mut tick = tokio::time::interval(FLUSH_EVERY);
    let mut cancelled = false;
    let flush = |buf: &mut String| {
        if !buf.is_empty() {
            let text = std::mem::take(buf);
            lock(&ctl.last_text).clone_from(&text);
            progress(exec, cid, WorkerUpdate::Text { text });
        }
    };
    let result = loop {
        tokio::select! {
            r = &mut run => break r,
            Some(e) = ev_rx.recv() => match e {
                WorkerEvent::Text(t) => buf.push_str(&t),
                WorkerEvent::ToolCall { id, title, kind } => {
                    flush(&mut buf);
                    progress(exec, cid, WorkerUpdate::ToolCall { tool_call_id: id, title, kind });
                }
                WorkerEvent::ToolCallDone { id, ok } => progress(exec, cid, WorkerUpdate::ToolCallDone { tool_call_id: id, ok }),
                WorkerEvent::Plan(entries) => progress(exec, cid, WorkerUpdate::Plan { entries }),
                WorkerEvent::Usage { used, size } => progress(exec, cid, WorkerUpdate::Usage { used, size }),
                WorkerEvent::TurnEnded { stop_reason } => {
                    flush(&mut buf);
                    let waiting = lock(&ctl.question).is_some() || !lock(&ctl.permissions).is_empty();
                    if ctl.is_completed() || cancelled || stop_reason == "cancelled" {
                        let _ = ctl_tx.send(Control::Stop);
                    } else if !waiting {
                        // Silence isn't success: ask the user how to continue.
                        let last = lock(&ctl.last_text).clone();
                        let tail: String = last.chars().rev().take(600).collect::<Vec<_>>().into_iter().rev().collect();
                        ctl.ask(
                            exec,
                            format!("The worker ended its turn without completing the task. Its last message:\n> {}\nWhat should it do next?", tail.trim()),
                        );
                    }
                }
            },
            Some(ask) = ask_rx.recv() => {
                flush(&mut buf);
                let did = DecisionId::generate();
                lock(&ctl.permissions).insert(did, ask.reply);
                exec.send(&ExecutorMsg::DecisionRequest {
                    command_id: cid,
                    decision_id: did,
                    question: format!("The worker wants to run: {}", ask.title),
                    options: ask.options.into_iter().map(|(id, label)| DecisionOption { id, label }).collect(),
                });
            }
            () = cancel.cancelled(), if !cancelled => {
                cancelled = true;
                let _ = ctl_tx.send(Control::Cancel);
                let _ = ctl_tx.send(Control::Stop);
            }
            _ = tick.tick() => flush(&mut buf),
        }
    };
    flush(&mut buf);
    exec.workers.remove(task_id);

    if cancelled {
        return CommandOutcome::Cancelled;
    }
    let completed = lock(&ctl.completed).clone();
    match (completed, result) {
        (Some(summary), _) => {
            let patch = match &base {
                Some(b) => match crate::capture_patch(root, b).await {
                    CommandOutcome::Succeeded { patch, .. } => patch,
                    _ => None,
                },
                None => None,
            };
            CommandOutcome::Succeeded {
                summary: Some(summary),
                patch,
            }
        }
        (None, Err(e)) => CommandOutcome::Failed {
            error: format!("worker stopped: {e}"),
        },
        (None, Ok(())) => CommandOutcome::Failed {
            error: "the worker stopped without completing the task".into(),
        },
    }
}
