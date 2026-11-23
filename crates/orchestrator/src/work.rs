//! Delegation, the per-turn state snapshot, inline-budget decisions, and starting held
//! tasks when a worker slot frees up.

use std::fmt::Write as _;
use std::str::FromStr as _;
use std::sync::Arc;

use chrono::Utc;
use console_agent::Brief;
use console_proto::api::TodoAddParams;
use console_proto::domain::{Score, SessionKind, TaskState, TodoKind};
use console_proto::event::EventBody;
use console_proto::link::{Capability, Op};
use console_proto::{TaskId, TodoId, TurnId};
use console_queue::Actor;
use console_store::{EventKeys, NewTask, TaskRecord};

use crate::App;
use crate::queue_ops;

fn score(v: Option<u8>) -> Option<Score> {
    v.and_then(|v| Score::try_from(v.clamp(1, 4)).ok())
}

/// Tasks that hold a worker slot: dispatched and not finished.
async fn active(app: &App) -> Vec<TaskRecord> {
    let undispatched: Vec<TaskId> = app
        .store
        .undispatched_tasks()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|t| t.id)
        .collect();
    app.store
        .tasks(false)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|t| !undispatched.contains(&t.id))
        .collect()
}

/// Why a new worker can't start now, if it can't.
async fn slot_problem(app: &App, task: &TaskRecord) -> Option<String> {
    let active = active(app).await;
    let limits = &app.config.orchestrator;
    let in_ws = active
        .iter()
        .filter(|t| t.workspace_id == task.workspace_id && t.id != task.id)
        .count();
    let total = active.iter().filter(|t| t.id != task.id).count();
    if in_ws >= limits.max_workers_per_workspace as usize {
        Some(format!(
            "a worker is already active in this workspace (limit {})",
            limits.max_workers_per_workspace
        ))
    } else if total >= limits.max_workers_total as usize {
        Some(format!(
            "{total} workers are already active (limit {})",
            limits.max_workers_total
        ))
    } else {
        None
    }
}

async fn start_worker(app: &App, task: &TaskRecord) -> Result<(), String> {
    let brief = match task.brief_artifact_id {
        Some(a) => app
            .store
            .artifact(a)
            .await
            .map_err(|e| e.to_string())?
            .map(|a| a.content),
        None => None,
    }
    .ok_or("the task has no brief")?;
    let op = Op::StartWorker {
        task_id: task.id,
        workspace_id: task.workspace_id,
        harness: task.harness.clone(),
        brief,
    };
    app.hub
        .start(app, task, op)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Starts held tasks, oldest first, while slots are free. Called when a task ends.
pub async fn start_held(app: &App) {
    for task in app.store.undispatched_tasks().await.unwrap_or_default() {
        if slot_problem(app, &task).await.is_none()
            && let Err(e) = start_worker(app, &task).await
        {
            tracing::warn!(task = %task.id, error = %e, "could not start a held task");
        }
    }
}

/// `delegate_task`: creates (or reuses) the todo, stores the brief, creates the task, and
/// starts a worker unless concurrency limits hold it back. Returns a message for the agent.
pub async fn delegate(
    app: &Arc<App>,
    brief: Brief,
    turn: Option<TurnId>,
) -> Result<String, String> {
    let ws = app
        .store
        .find_workspace(brief.workspace.trim())
        .await
        .map_err(|e| e.to_string())?;
    let Some(ws) = ws else {
        let names: Vec<String> = app
            .store
            .workspaces()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|w| w.name)
            .collect();
        return Err(format!(
            "unknown workspace {:?}; known workspaces: {}",
            brief.workspace,
            names.join(", ")
        ));
    };
    let harness = brief
        .harness
        .clone()
        .unwrap_or_else(|| app.config.executor.default_harness.clone());
    let supported = app
        .store
        .executors()
        .await
        .unwrap_or_default()
        .into_iter()
        .find(|e| e.id == ws.executor_id)
        .is_some_and(|e| {
            e.capabilities.contains(&Capability::Worker {
                harness: harness.clone(),
            })
        });
    if !supported {
        return Err(format!(
            "the executor for {} can't run {harness:?} workers",
            ws.name
        ));
    }

    // The todo this work belongs to.
    let todo_id = match brief.todo_id.as_deref().map(TodoId::from_str) {
        Some(Ok(id)) => match app.store.todo(id).await.map_err(|e| e.to_string())? {
            Some(t) => t.id,
            None => return Err(format!("no todo {id}")),
        },
        Some(Err(_)) => return Err(format!("{:?} is not a todo ID", brief.todo_id)),
        None => {
            queue_ops::add(
                app,
                TodoAddParams {
                    title: brief.title.clone(),
                    body: Some(brief.goal.clone()),
                    kind: Some(TodoKind::Feature),
                    importance: score(brief.importance),
                    urgency: score(brief.urgency),
                    due_at: None,
                    workspace_id: Some(ws.id),
                },
                Actor::Orchestrator,
            )
            .await
            .map_err(|e| e.0.message)?
            .id
        }
    };

    let task_id = TaskId::generate();
    let rendered = console_agent::render_brief(&brief, &task_id.to_string());
    let key = turn.map(|t| format!("{t}:{}", brief.title));
    let orch = app.orchestrator_session;
    let title = brief.title.clone();
    let (task, created) = app
        .store
        .write(async |tx| {
            let brief_id = tx.insert_artifact(None, "brief", &rendered).await?;
            let (task, created) = tx
                .insert_task(NewTask {
                    id: task_id,
                    todo_id,
                    title: title.clone(),
                    workspace_id: ws.id,
                    harness: harness.clone(),
                    origin_turn_id: turn,
                    brief_artifact_id: Some(brief_id),
                    idempotency_key: key,
                })
                .await?;
            if created {
                tx.link_artifact(brief_id, task.id).await?;
                let session = tx
                    .insert_session(SessionKind::Worker, Some(task.id), &title)
                    .await?;
                tx.set_task_worker_session(task.id, session.id).await?;
                tx.append(
                    EventKeys::task(task.id)
                        .with_todo(todo_id)
                        .with_session(orch),
                    EventBody::TaskDelegated {
                        title: title.clone(),
                        workspace_id: ws.id,
                    },
                )
                .await?;
            }
            Ok((task, created))
        })
        .await
        .map_err(|e| e.to_string())?;
    if !created {
        return Ok(format!(
            "Already delegated as task {} ({:?}).",
            task.id, task.state
        ));
    }
    app.agent.mark_delegated();

    if let Some(problem) = slot_problem(app, &task).await {
        return Ok(format!(
            "Created task {} (\"{}\") but held it: {problem}. It starts automatically when a slot frees up. Tell the user.",
            task.id, task.title
        ));
    }
    start_worker(app, &task).await?;
    let task = app.store.task(task.id).await.ok().flatten().unwrap_or(task);
    let note = if task.blocked.is_some() {
        " The executor is offline, so it will start when it reconnects."
    } else {
        ""
    };
    Ok(format!(
        "Delegated as task {} (\"{}\") to a {harness} worker in {}.{note} Tell the user in one line and end your turn.",
        task.id, task.title, ws.name
    ))
}

fn state_label(t: &TaskRecord) -> String {
    let mut s = serde_json::to_value(t.state)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    if let Some(b) = t.blocked {
        let _ = write!(
            s,
            " ({})",
            serde_json::to_value(b)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default()
        );
    }
    s
}

/// The state injected at the start of every turn.
pub async fn snapshot(app: &App) -> String {
    let now = Utc::now();
    let mut out = format!("STATE SNAPSHOT ({} UTC)\n", now.format("%Y-%m-%d %H:%M"));

    let queue = queue_ops::list(app, false).await.unwrap_or_default();
    let _ = writeln!(
        out,
        "\nQueue (top {} of {}):",
        queue.len().min(crate::QUEUE_TOP),
        queue.len()
    );
    if queue.is_empty() {
        out.push_str("  (empty)\n");
    }
    for (i, t) in queue.iter().take(crate::QUEUE_TOP).enumerate() {
        let status = serde_json::to_value(t.status)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "  {}. {:?} {status}: {} [{}] — {}",
            i + 1,
            t.band,
            t.title,
            t.id,
            t.why
        );
    }

    let tasks = app.store.tasks(false).await.unwrap_or_default();
    out.push_str("\nActive tasks:\n");
    if tasks.is_empty() {
        out.push_str("  (none)\n");
    }
    for t in &tasks {
        let _ = writeln!(
            out,
            "  - {} \"{}\" ({} worker): {}",
            t.id,
            t.title,
            t.harness,
            state_label(t)
        );
    }

    let decisions = app.store.open_decisions().await.unwrap_or_default();
    let unknown: Vec<_> = tasks
        .iter()
        .filter(|t| t.state == TaskState::OutcomeUnknown)
        .collect();
    out.push_str("\nWaiting on the user:\n");
    if decisions.is_empty() && unknown.is_empty() {
        out.push_str("  (nothing)\n");
    }
    for d in &decisions {
        let opts: Vec<String> = d
            .options
            .iter()
            .map(|o| format!("{} ({})", o.id, o.label))
            .collect();
        let _ = writeln!(
            out,
            "  - decision {} for task {}: {} Options: {}",
            d.id,
            d.task_id,
            d.question,
            if opts.is_empty() {
                "free-text answer".into()
            } else {
                opts.join(", ")
            }
        );
    }
    for t in unknown {
        let _ = writeln!(
            out,
            "  - task {} \"{}\": outcome unknown after an interruption; ask whether to retry or how to settle it",
            t.id, t.title
        );
    }

    // Recently finished: terminal tasks from the last day, newest first.
    let day_ago = now - chrono::Duration::hours(24);
    let finished: Vec<_> = app
        .store
        .tasks(true)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|t| t.state.is_terminal() && t.updated_at > day_ago)
        .take(5)
        .collect();
    if !finished.is_empty() {
        out.push_str("\nFinished in the last 24 hours:\n");
        for t in finished {
            let summary = t
                .summary
                .as_deref()
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("");
            let _ = writeln!(
                out,
                "  - {} \"{}\": {} {summary}",
                t.id,
                t.title,
                state_label(&t)
            );
        }
    }

    let proposals = app
        .store
        .open_proposals()
        .await
        .map(|p| p.len())
        .unwrap_or(0);
    if proposals > 0 {
        let _ = writeln!(out, "\nOpen proposals awaiting the user: {proposals}");
    }

    let workspaces = app.workspace_views().await.unwrap_or_default();
    out.push_str("\nWorkspaces: ");
    if workspaces.is_empty() {
        out.push_str("(none registered; an executor must be running)");
    }
    let ws: Vec<String> = workspaces
        .iter()
        .map(|w| {
            format!(
                "{} ({}{})",
                w.name,
                w.executor_name,
                if w.online { "" } else { ", offline" }
            )
        })
        .collect();
    out.push_str(&ws.join(", "));
    out.push('\n');
    out
}

/// The inline-budget decision for a tool call in the current turn. `None` allows it.
pub async fn pre_tool(app: &App, tool_name: &str) -> Option<String> {
    if console_agent::is_delegate(tool_name) {
        return None;
    }
    let (turn, started) = app.agent.current()?;
    let calls = app
        .store
        .write(async |tx| tx.bump_tool_calls(turn).await)
        .await
        .ok()?;
    let limits = &app.config.agent;
    let over_calls = calls > limits.inline_max_tool_calls;
    let over_time = started.elapsed().as_secs() > limits.inline_max_wall_secs;
    if !(over_calls || over_time) {
        return None;
    }
    let _ = app
        .store
        .write(async |tx| tx.mark_budget_hit(turn).await)
        .await;
    let what = if over_calls {
        format!("{} tool calls", limits.inline_max_tool_calls)
    } else {
        format!("{} seconds", limits.inline_max_wall_secs)
    };
    Some(format!(
        "Inline budget exhausted ({what} this turn). Stop looking things up: answer with what you have, or call delegate_task with your findings in the brief's context."
    ))
}
