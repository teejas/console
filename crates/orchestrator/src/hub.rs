//! The executor hub: the `/executor` link endpoint, dispatch, results, reconciliation,
//! and read ops (`docs/specs/executors.md`).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use console_proto::domain::TaskState;
use console_proto::link::{
    CommandOutcome, CommandStatus, ExecutorMsg, JournalEntry, LINK_PROTOCOL_VERSION, Op,
    OrchestratorMsg, ReadOp, ReadOpOutcome,
};
use console_proto::{AuthorityEpoch, CommandId, ExecutorId, TaskId, WorkspaceId};
use console_store::{CommandRecord, StoreError, TaskRecord};
use futures::{SinkExt as _, StreamExt as _};
use tokio::sync::{mpsc, oneshot};

use crate::App;
use crate::engine;
use crate::task_machine::Input;

/// The authority epoch this installation dispatches with.
pub const EPOCH: AuthorityEpoch = AuthorityEpoch(1);

#[derive(Debug, Default)]
pub struct Hub {
    conns: std::sync::Mutex<HashMap<ExecutorId, Link>>,
    pending_reads: std::sync::Mutex<HashMap<String, oneshot::Sender<ReadOpOutcome>>>,
    next_request: AtomicU64,
    next_link: AtomicU64,
}

#[derive(Debug, Clone)]
struct Link {
    serial: u64,
    out: mpsc::UnboundedSender<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum HubError {
    #[error("no workspace {0}")]
    UnknownWorkspace(String),
    #[error("the executor for this workspace is offline")]
    Offline,
    #[error("timed out waiting for the executor")]
    Timeout,
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Engine(#[from] engine::EngineError),
}

impl Hub {
    fn send(&self, eid: ExecutorId, msg: &OrchestratorMsg) -> bool {
        let Ok(text) = serde_json::to_string(msg) else {
            return false;
        };
        self.conns
            .lock()
            .ok()
            .and_then(|c| c.get(&eid).map(|l| l.out.send(text).is_ok()))
            .unwrap_or(false)
    }

    #[must_use]
    pub fn is_online(&self, eid: ExecutorId) -> bool {
        self.conns.lock().is_ok_and(|c| c.contains_key(&eid))
    }

    /// Runs a read op on the executor that owns the workspace.
    pub async fn read_op(
        &self,
        app: &App,
        workspace_id: WorkspaceId,
        op: ReadOp,
        deadline: Duration,
    ) -> Result<ReadOpOutcome, HubError> {
        let ws = app
            .store
            .workspace(workspace_id)
            .await?
            .ok_or_else(|| HubError::UnknownWorkspace(workspace_id.to_string()))?;
        let request_id = format!("r{}", self.next_request.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        if let Ok(mut p) = self.pending_reads.lock() {
            p.insert(request_id.clone(), tx);
        }
        let deadline_ms = u64::try_from(deadline.as_millis()).unwrap_or(u64::MAX);
        if !self.send(
            ws.executor_id,
            &OrchestratorMsg::ReadOp {
                request_id: request_id.clone(),
                op,
                deadline_ms,
            },
        ) {
            if let Ok(mut p) = self.pending_reads.lock() {
                p.remove(&request_id);
            }
            return Err(HubError::Offline);
        }
        let outcome = tokio::time::timeout(deadline + Duration::from_secs(2), rx).await;
        if let Ok(mut p) = self.pending_reads.lock() {
            p.remove(&request_id);
        }
        match outcome {
            Ok(Ok(r)) => Ok(r),
            _ => Err(HubError::Timeout),
        }
    }

    /// Starts a new attempt for a task: records the attempt and command (dispatch intent),
    /// then sends it if the executor is online. Offline, the task waits.
    pub async fn start(
        &self,
        app: &App,
        task: &TaskRecord,
        op: Op,
    ) -> Result<CommandRecord, HubError> {
        let ws = app
            .store
            .workspace(task.workspace_id)
            .await?
            .ok_or_else(|| HubError::UnknownWorkspace(task.workspace_id.to_string()))?;
        let eid = ws.executor_id;
        let task_id = task.id;
        let cmd = app
            .store
            .write(async |tx| {
                let aid = tx.insert_attempt(task_id, eid).await?;
                tx.insert_command(eid, Some(task_id), Some(aid), &op).await
            })
            .await?;
        if !self.dispatch(&cmd) {
            engine::apply(&app.store, task.id, Input::ExecutorOffline).await?;
        }
        Ok(cmd)
    }

    fn dispatch(&self, cmd: &CommandRecord) -> bool {
        let Some(attempt_id) = cmd.attempt_id else {
            return false;
        };
        self.send(
            cmd.executor_id,
            &OrchestratorMsg::Dispatch {
                command_id: cmd.id,
                attempt_id,
                epoch: EPOCH,
                op: cmd.op.clone(),
            },
        )
    }

    /// Dispatches an op that isn't a task attempt (for example a prompt to a running
    /// worker). Its status never moves a task.
    pub async fn send_untracked(&self, app: &App, eid: ExecutorId, op: Op) -> bool {
        let Ok(mut cmd) = app
            .store
            .write(async |tx| tx.insert_command(eid, None, None, &op).await)
            .await
        else {
            return false;
        };
        cmd.attempt_id = Some(console_proto::AttemptId::generate());
        self.dispatch(&cmd)
    }

    /// Asks the executor to stop the task's current command, if it is reachable.
    pub async fn cancel(&self, app: &App, task_id: TaskId) -> Result<bool, HubError> {
        let Some(cmd) = app.store.latest_command_for_task(task_id).await? else {
            return Ok(false);
        };
        Ok(self.send(
            cmd.executor_id,
            &OrchestratorMsg::Cancel {
                command_id: cmd.id,
                epoch: EPOCH,
            },
        ))
    }

    /// Sends a raw message to an executor (used by later milestones for worker traffic).
    pub fn send_to(&self, eid: ExecutorId, msg: &OrchestratorMsg) -> bool {
        self.send(eid, msg)
    }
}

pub async fn executor_ws(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Response {
    if !crate::server::authorized(&app, &headers, &query) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    ws.on_upgrade(move |socket| executor_conn(app, socket))
}

async fn executor_conn(app: Arc<App>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let hello = loop {
        match stream.next().await {
            Some(Ok(Message::Text(t))) => match serde_json::from_str::<ExecutorMsg>(&t) {
                Ok(m @ ExecutorMsg::Hello { .. }) => break m,
                _ => return,
            },
            Some(Ok(Message::Close(_)) | Err(_)) | None => return,
            Some(Ok(_)) => {}
        }
    };
    let ExecutorMsg::Hello {
        executor_id,
        name,
        protocol,
        max_epoch_seen,
        capabilities,
        workspaces,
        journal,
    } = hello
    else {
        return;
    };
    let reject = |reason: String| OrchestratorMsg::Reject { reason };
    let refusal = if protocol != LINK_PROTOCOL_VERSION {
        Some(format!(
            "link protocol {protocol} is not {LINK_PROTOCOL_VERSION}"
        ))
    } else if max_epoch_seen > EPOCH {
        Some(format!("this Orchestrator's epoch {} is stale", EPOCH.0))
    } else {
        None
    };
    if let Some(reason) = refusal {
        tracing::warn!(executor = %executor_id, %reason, "rejecting executor");
        let _ = sink
            .send(Message::Text(
                serde_json::to_string(&reject(reason))
                    .unwrap_or_default()
                    .into(),
            ))
            .await;
        return;
    }

    if let Err(e) = app
        .store
        .write(async |tx| {
            tx.executor_online(executor_id, &name, &capabilities, &workspaces)
                .await
        })
        .await
    {
        tracing::error!(error = %e, "could not register executor");
        return;
    }
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let serial = app.hub.next_link.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut c) = app.hub.conns.lock() {
        c.insert(
            executor_id,
            Link {
                serial,
                out: out_tx.clone(),
            },
        );
    }
    let welcome = OrchestratorMsg::Welcome {
        epoch: EPOCH,
        protocol: LINK_PROTOCOL_VERSION,
    };
    let _ = out_tx.send(serde_json::to_string(&welcome).unwrap_or_default());
    let writer = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            if sink.send(Message::Text(m.into())).await.is_err() {
                break;
            }
        }
    });
    tracing::info!(executor = %executor_id, %name, workspaces = workspaces.len(), "executor connected");

    reconcile(&app, executor_id, &journal).await;

    while let Some(Ok(msg)) = stream.next().await {
        let text = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };
        match serde_json::from_str::<ExecutorMsg>(&text) {
            Ok(m) => handle(&app, executor_id, m).await,
            Err(e) => tracing::warn!(error = %e, "bad message from executor"),
        }
    }

    writer.abort();
    let still_current = app
        .hub
        .conns
        .lock()
        .map(|mut c| {
            if c.get(&executor_id).is_some_and(|l| l.serial == serial) {
                c.remove(&executor_id);
                true
            } else {
                false
            }
        })
        .unwrap_or(false);
    if still_current {
        tracing::info!(executor = %executor_id, "executor disconnected");
        let _ = app
            .store
            .write(async |tx| tx.executor_offline(executor_id).await)
            .await;
        if let Ok(cmds) = app.store.open_commands_for(executor_id).await {
            for task_id in cmds.iter().filter_map(|c| c.task_id) {
                let _ = engine::apply(&app.store, task_id, Input::ExecutorOffline).await;
            }
        }
    }
}

/// Brings the Orchestrator's view of an executor's commands in line with its journal.
async fn reconcile(app: &App, eid: ExecutorId, journal: &[JournalEntry]) {
    let by_id: HashMap<CommandId, CommandStatus> =
        journal.iter().map(|e| (e.command_id, e.status)).collect();
    let Ok(open) = app.store.open_commands_for(eid).await else {
        return;
    };
    let mut ask = Vec::new();
    for cmd in open {
        match (by_id.get(&cmd.id), cmd.status) {
            (Some(&status), _) if status != CommandStatus::Unknown => {
                apply_status(app, &cmd, status, None).await;
                if let Some(task_id) = cmd.task_id {
                    let _ = engine::apply(&app.store, task_id, Input::ExecutorOnline).await;
                    resend_cancel_if_requested(app, &cmd, task_id).await;
                }
            }
            // Never acknowledged: dispatching again is safe, the journal deduplicates.
            (_, None) => {
                if let Some(task_id) = cmd.task_id {
                    let _ = engine::apply(&app.store, task_id, Input::ExecutorOnline).await;
                }
                app.hub.dispatch(&cmd);
            }
            // We think it was accepted, but the journal didn't mention it: ask.
            (_, Some(_)) => ask.push(cmd.id),
        }
    }
    if !ask.is_empty() {
        app.hub
            .send(eid, &OrchestratorMsg::Reconcile { command_ids: ask });
    }
}

async fn resend_cancel_if_requested(app: &App, cmd: &CommandRecord, task_id: TaskId) {
    if let Ok(Some(task)) = app.store.task(task_id).await
        && task.state == TaskState::CancelRequested
    {
        app.hub.send(
            cmd.executor_id,
            &OrchestratorMsg::Cancel {
                command_id: cmd.id,
                epoch: EPOCH,
            },
        );
    }
}

async fn handle(app: &App, eid: ExecutorId, msg: ExecutorMsg) {
    match msg {
        ExecutorMsg::Accepted { command_id } => {
            if let Ok(Some(cmd)) = app.store.command(command_id).await {
                apply_status(app, &cmd, CommandStatus::Accepted, None).await;
            }
        }
        ExecutorMsg::Duplicate { command_id, status } => {
            if let Ok(Some(cmd)) = app.store.command(command_id).await {
                apply_status(app, &cmd, status, None).await;
            }
        }
        ExecutorMsg::Result {
            command_id,
            outcome,
        } => {
            if let Ok(Some(cmd)) = app.store.command(command_id).await {
                let status = match &outcome {
                    CommandOutcome::Succeeded { .. } => CommandStatus::Succeeded,
                    CommandOutcome::Failed { .. } => CommandStatus::Failed,
                    CommandOutcome::Cancelled => CommandStatus::Cancelled,
                    CommandOutcome::Interrupted => CommandStatus::Interrupted,
                };
                apply_status(app, &cmd, status, Some(&outcome)).await;
            }
        }
        ExecutorMsg::ReconcileReport { entries } => {
            for e in entries {
                if let Ok(Some(cmd)) = app.store.command(e.command_id).await {
                    if e.status == CommandStatus::Unknown && cmd.status.is_none() {
                        app.hub.dispatch(&cmd);
                    } else {
                        apply_status(app, &cmd, e.status, None).await;
                    }
                }
            }
        }
        ExecutorMsg::ReadOpResult { request_id, result } => {
            let tx = app
                .hub
                .pending_reads
                .lock()
                .ok()
                .and_then(|mut p| p.remove(&request_id));
            if let Some(tx) = tx {
                let _ = tx.send(result);
            }
        }
        other => app.on_worker_message(eid, other).await,
    }
}

/// Records a command's status and moves its task accordingly.
async fn apply_status(
    app: &App,
    cmd: &CommandRecord,
    status: CommandStatus,
    outcome: Option<&CommandOutcome>,
) {
    if cmd.is_final() {
        return; // Already settled; redelivery is a no-op.
    }
    let cid = cmd.id;
    let _ = app
        .store
        .write(async |tx| tx.set_command_status(cid, status).await)
        .await;
    let Some(task_id) = cmd.task_id else { return };
    let Ok(Some(task)) = app.store.task(task_id).await else {
        return;
    };
    let unknown = task.state == TaskState::OutcomeUnknown;
    let input = match status {
        CommandStatus::Accepted | CommandStatus::Running => Some(Input::Accepted),
        CommandStatus::Succeeded if unknown => Some(Input::ReconciledSucceeded),
        CommandStatus::Succeeded => Some(Input::Succeeded),
        CommandStatus::Failed if unknown => Some(Input::ReconciledFailed),
        CommandStatus::Failed => Some(Input::Failed),
        CommandStatus::Cancelled if unknown => Some(Input::ReconciledCancelled),
        CommandStatus::Cancelled => Some(Input::CancelConfirmed),
        CommandStatus::Interrupted => Some(Input::OutcomeLost),
        CommandStatus::Unknown => None,
    };
    if matches!(
        status,
        CommandStatus::Succeeded
            | CommandStatus::Failed
            | CommandStatus::Cancelled
            | CommandStatus::Interrupted
    ) {
        record_outcome(app, cmd, task_id, status, outcome).await;
    }
    if let Some(input) = input
        && let Err(e) = engine::apply(&app.store, task_id, input).await
    {
        tracing::warn!(task = %task_id, ?input, error = %e, "executor report did not apply");
    }
    app.after_command_settled(cmd, status).await;
}

async fn record_outcome(
    app: &App,
    cmd: &CommandRecord,
    task_id: TaskId,
    status: CommandStatus,
    outcome: Option<&CommandOutcome>,
) {
    let label = match status {
        CommandStatus::Succeeded => "succeeded",
        CommandStatus::Failed => "failed",
        CommandStatus::Cancelled => "cancelled",
        _ => "interrupted",
    };
    let attempt = cmd.attempt_id;
    let _ = app
        .store
        .write(async |tx| {
            if let Some(aid) = attempt {
                tx.end_attempt(aid, label).await?;
            }
            match outcome {
                Some(CommandOutcome::Succeeded { summary, patch }) => {
                    if let Some(s) = summary {
                        tx.set_task_summary(task_id, s).await?;
                    }
                    if let Some(p) = patch.as_deref().filter(|p| !p.is_empty()) {
                        tx.insert_artifact(Some(task_id), "patch", p).await?;
                    }
                }
                Some(CommandOutcome::Failed { error }) => {
                    tx.set_task_summary(task_id, error).await?
                }
                _ => {}
            }
            Ok(())
        })
        .await;
}
