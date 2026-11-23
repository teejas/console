//! Worker traffic between executors and the Orchestrator: progress, questions,
//! permissions, follow-up proposals, answers, and steering.

use console_proto::api::TodoAddParams;
use console_proto::domain::{BlockedReason, TodoKind};
use console_proto::event::EventBody;
use console_proto::link::{ExecutorMsg, Op, OrchestratorMsg, WorkerUpdate};
use console_proto::{ExecutorId, TaskId};
use console_queue::Actor;
use console_store::{DecisionOrigin, DecisionRecord, EventKeys};

use crate::task_machine::Input;
use crate::{App, engine, queue_ops};

/// Handles worker messages from an executor.
pub async fn on_message(app: &App, eid: ExecutorId, msg: ExecutorMsg) {
    match msg {
        ExecutorMsg::Progress { command_id, update } => {
            let Ok(Some(cmd)) = app.store.command(command_id).await else {
                return;
            };
            let Some(task_id) = cmd.task_id else { return };
            let Ok(Some(session)) = app.store.worker_session_for(task_id).await else {
                return;
            };
            let body = match update {
                WorkerUpdate::Text { text } => EventBody::AgentText {
                    turn_id: None,
                    text,
                },
                WorkerUpdate::ToolCall {
                    tool_call_id,
                    title,
                    ..
                } => EventBody::ToolCallStarted {
                    turn_id: None,
                    tool_call_id,
                    title,
                },
                WorkerUpdate::ToolCallDone { tool_call_id, ok } => EventBody::ToolCallFinished {
                    tool_call_id,
                    ok,
                    summary: None,
                },
                WorkerUpdate::Plan { .. } | WorkerUpdate::Usage { .. } => return,
            };
            let keys = EventKeys::session(session.id).with_task(task_id);
            let _ = app
                .store
                .write(async |tx| tx.append(keys, body).await)
                .await;
        }
        ExecutorMsg::DecisionRequest {
            command_id,
            decision_id,
            question,
            options,
        } => {
            let Ok(Some(cmd)) = app.store.command(command_id).await else {
                return;
            };
            let Some(task_id) = cmd.task_id else { return };
            let (origin, reason) = if options.is_empty() {
                (DecisionOrigin::Question, BlockedReason::AwaitingInput)
            } else {
                (DecisionOrigin::Permission, BlockedReason::AwaitingDecision)
            };
            let q = question.clone();
            let stored = app
                .store
                .write(async |tx| {
                    tx.insert_decision(decision_id, task_id, origin, &q, options)
                        .await
                })
                .await;
            if let Err(e) = stored {
                tracing::warn!(error = %e, "could not record a worker decision");
                return;
            }
            if let Err(e) = engine::apply(&app.store, task_id, Input::Blocked(reason)).await {
                tracing::warn!(task = %task_id, error = %e, "could not block task on decision");
            }
        }
        ExecutorMsg::McpCall {
            call_id,
            command_id,
            tool,
            args,
        } => {
            let (result, is_error) = match mcp_call(app, command_id, &tool, &args).await {
                Ok(text) => (serde_json::Value::String(text), false),
                Err(e) => (serde_json::Value::String(e), true),
            };
            app.hub.send_to(
                eid,
                &OrchestratorMsg::McpResult {
                    call_id,
                    result,
                    is_error,
                },
            );
        }
        other => tracing::debug!(?other, "unhandled executor message"),
    }
}

async fn mcp_call(
    app: &App,
    command_id: console_proto::CommandId,
    tool: &str,
    args: &serde_json::Value,
) -> Result<String, String> {
    match tool {
        "propose_followup" => {
            let title = args["title"].as_str().unwrap_or("").trim().to_owned();
            if title.is_empty() {
                return Err("a follow-up needs a title".into());
            }
            let why = args["why"].as_str().unwrap_or("").to_owned();
            let workspace_id = match app
                .store
                .command(command_id)
                .await
                .map_err(|e| e.to_string())?
            {
                Some(cmd) => match cmd.op {
                    Op::StartWorker { workspace_id, .. } => Some(workspace_id),
                    _ => None,
                },
                None => None,
            };
            let t = queue_ops::add(
                app,
                TodoAddParams {
                    title,
                    body: Some(why).filter(|w| !w.is_empty()),
                    kind: Some(TodoKind::Chore),
                    importance: None,
                    urgency: None,
                    due_at: None,
                    workspace_id,
                },
                Actor::Orchestrator,
            )
            .await
            .map_err(|e| e.0.message)?;
            Ok(format!(
                "Added to the user's queue as \"{}\" ({:?}).",
                t.title, t.band
            ))
        }
        other => Err(format!("unknown tool {other}")),
    }
}

/// Sends the user's answer to the worker and unblocks the task when nothing else waits.
pub async fn route_answer(app: &App, decision: &DecisionRecord, answer: Option<&str>) {
    let task_id = decision.task_id;
    if let Ok(Some(cmd)) = app.store.latest_command_for_task(task_id).await {
        app.hub.send_to(
            cmd.executor_id,
            &OrchestratorMsg::DecisionResponse {
                decision_id: decision.id,
                option_id: decision
                    .option_id
                    .clone()
                    .unwrap_or_else(|| "answer".into()),
                answer: answer.map(str::to_owned),
            },
        );
    }
    let still_open = app
        .store
        .open_decisions()
        .await
        .unwrap_or_default()
        .iter()
        .any(|d| d.task_id == task_id);
    if !still_open {
        let _ = engine::apply(&app.store, task_id, Input::Unblocked).await;
    }
}

/// Sends a message to a task's running worker (an answer or steering).
pub async fn message_worker(app: &App, task_id: TaskId, text: &str) -> Result<String, String> {
    let task = app
        .store
        .task(task_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("no such task")?;
    if task.state.is_terminal() {
        return Err(format!(
            "\"{}\" has already finished ({:?})",
            task.title, task.state
        ));
    }
    let cmd = app
        .store
        .latest_command_for_task(task_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("the task hasn't started a worker yet")?;
    let op = Op::WorkerPrompt {
        task_id,
        text: text.to_owned(),
    };
    if !app.hub.send_untracked(app, cmd.executor_id, op).await {
        return Err("the worker's executor is offline".into());
    }
    if let Ok(Some(session)) = app.store.worker_session_for(task_id).await {
        let t = text.to_owned();
        let keys = EventKeys::session(session.id).with_task(task_id);
        let _ = app
            .store
            .write(async |tx| {
                tx.append(
                    keys,
                    EventBody::UserMessage {
                        turn_id: console_proto::TurnId::generate(),
                        text: t,
                    },
                )
                .await
            })
            .await;
    }
    Ok(format!("Sent to the worker on \"{}\".", task.title))
}
