//! `inspect`: a readable account of a task, turn, or todo.

use std::fmt::Write as _;
use std::str::FromStr as _;

use console_proto::{TaskId, TodoId, TurnId};

use crate::App;

fn enum_str<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Resolves an ID or unique suffix and describes it.
pub async fn inspect(app: &App, reference: &str) -> Result<String, String> {
    let reference = reference.trim();
    let id = if reference.contains('_') {
        reference.to_owned()
    } else {
        let found = app
            .store
            .find_by_suffix(reference)
            .await
            .map_err(|e| e.to_string())?;
        match found.as_slice() {
            [one] => one.clone(),
            [] => return Err(format!("nothing matches {reference:?}")),
            _ => {
                return Err(format!(
                    "{reference:?} matches {} items; use more of the ID",
                    found.len()
                ));
            }
        }
    };
    if let Ok(t) = TaskId::from_str(&id) {
        return task(app, t).await;
    }
    if let Ok(t) = TurnId::from_str(&id) {
        return turn(app, t).await;
    }
    if let Ok(t) = TodoId::from_str(&id) {
        let todo = app
            .store
            .todo(t)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("no such todo")?;
        let view = crate::queue_ops::get(app, t)
            .await
            .map_err(|e| e.0.message)?;
        let mut out = format!("Todo {} \"{}\"\n", todo.id, todo.title);
        let _ = writeln!(
            out,
            "  {:?}, {}, importance {}, urgency {}",
            view.band,
            enum_str(&todo.status),
            todo.importance.get(),
            todo.urgency.get()
        );
        let _ = writeln!(out, "  why: {}", view.why);
        if !todo.user_fields.is_empty() {
            let _ = writeln!(out, "  fields you own: {}", todo.user_fields.join(", "));
        }
        for task in app
            .store
            .tasks(true)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|x| x.todo_id == t)
        {
            let _ = writeln!(out, "  task {} — {}", task.id, enum_str(&task.state));
        }
        return Ok(out);
    }
    Err(format!("{id:?} is not a task, turn, or todo ID"))
}

async fn task(app: &App, id: TaskId) -> Result<String, String> {
    let t = app
        .store
        .task(id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("no such task")?;
    let ws = app.store.workspace(t.workspace_id).await.ok().flatten();
    let mut out = format!("Task {} \"{}\"\n", t.id, t.title);
    let _ = writeln!(
        out,
        "  state: {}{}",
        enum_str(&t.state),
        t.blocked
            .map(|b| format!(" ({})", enum_str(&b)))
            .unwrap_or_default()
    );
    let _ = writeln!(
        out,
        "  worker: {} in {}",
        t.harness,
        ws.map_or_else(|| t.workspace_id.to_string(), |w| w.name)
    );
    let _ = writeln!(
        out,
        "  created {}, updated {}",
        t.created_at.format("%F %T"),
        t.updated_at.format("%F %T")
    );
    if let Some(turn) = t.origin_turn_id {
        let _ = writeln!(out, "  delegated in turn {turn}");
    }
    let attempts = app.store.attempts_for(id).await.unwrap_or_default();
    let _ = writeln!(out, "  attempts: {}", attempts.len());
    for (i, a) in attempts.iter().enumerate() {
        let took = a
            .ended_at
            .map(|e| format!(", {}s", (e - a.created_at).num_seconds()))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "    {}. {} {}{took}",
            i + 1,
            a.created_at.format("%T"),
            a.outcome.as_deref().unwrap_or("running")
        );
    }
    for d in app.store.decisions_for(id).await.unwrap_or_default() {
        let answer = d
            .answer
            .clone()
            .or(d.option_id.clone())
            .unwrap_or_else(|| "(open)".into());
        let _ = writeln!(
            out,
            "  decision: {} → {answer}",
            d.question.lines().next().unwrap_or("")
        );
    }
    if let Some(s) = &t.summary {
        let _ = writeln!(out, "  summary: {s}");
    }
    if let Ok(Some(p)) = app.store.latest_artifact(id, "patch").await {
        let files = p
            .content
            .lines()
            .filter(|l| l.starts_with("diff --git"))
            .count();
        let _ = writeln!(
            out,
            "  patch: {files} file(s), {} lines",
            p.content.lines().count()
        );
    }
    Ok(out)
}

async fn turn(app: &App, id: TurnId) -> Result<String, String> {
    let t = app
        .store
        .turn(id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("no such turn")?;
    let mut out = format!(
        "Turn {}\n  message: {}\n",
        t.id,
        t.user_message.chars().take(300).collect::<String>()
    );
    let _ = writeln!(
        out,
        "  {}: {}, {} tool calls, {}",
        t.status,
        t.decision.map_or_else(|| "-".into(), |d| enum_str(&d)),
        t.tool_calls,
        t.wall_ms
            .map_or_else(|| "-".into(), |w| format!("{:.1}s", w as f64 / 1000.0))
    );
    if t.budget_hit {
        out.push_str("  hit the inline budget\n");
    }
    if let Some(o) = t.override_mode {
        let _ = writeln!(out, "  overridden: {}", enum_str(&o));
    }
    if let Some(c) = t.cost_usd {
        let _ = writeln!(out, "  cost: ${c:.4}");
    }
    let delegated: Vec<_> = app
        .store
        .tasks(true)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|x| x.origin_turn_id == Some(id))
        .collect();
    for d in delegated {
        let _ = writeln!(
            out,
            "  delegated: {} \"{}\" — {}",
            d.id,
            d.title,
            enum_str(&d.state)
        );
    }
    Ok(out)
}
