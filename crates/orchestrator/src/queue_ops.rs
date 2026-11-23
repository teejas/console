//! Queue operations over the store, using the pure rules in `console-queue`.
//!
//! The client API acts as the user; the Orchestrator's tools act as the Orchestrator, so
//! their edits to user-owned fields become proposals.

use chrono::{Duration, Utc};
use console_proto::api::{
    ProposalView, TodoAddParams, TodoUpdateParams, TodoUpdateResult, TodoView,
};
use console_proto::domain::{Score, TodoKind, TodoOwner, TodoStatus};
use console_proto::event::EventBody;
use console_proto::{ProposalId, TodoId};
use console_queue::{Actor, Change, Item, Ranked};
use console_store::{EventKeys, NewTodo, ProposalRecord, StoreError, TodoRecord, WriteTx};

use crate::App;
use crate::api::{ApiError, ApiResult};
use console_proto::rpc::RpcError;

/// Proposals the user ignores expire after this long.
const PROPOSAL_TTL_HOURS: i64 = 24;

fn manual(rank: Option<&str>) -> Option<f64> {
    rank.and_then(|r| r.parse().ok())
}

pub(crate) fn to_item(rec: &TodoRecord) -> Item {
    Item {
        id: rec.id,
        title: rec.title.clone(),
        kind: rec.kind,
        owner: rec.owner,
        status: rec.status,
        importance: rec.importance,
        urgency: rec.urgency,
        due_at: rec.due_at,
        manual_rank: manual(rec.manual_rank.as_deref()),
        workspace_id: rec.workspace_id,
        user_fields: rec.user_fields.clone(),
        created_at: rec.created_at,
        unblocks: 0,
    }
}

fn apply_item(rec: &mut TodoRecord, item: &Item) {
    rec.title.clone_from(&item.title);
    rec.kind = item.kind;
    rec.owner = item.owner;
    rec.status = item.status;
    rec.importance = item.importance;
    rec.urgency = item.urgency;
    rec.due_at = item.due_at;
    rec.workspace_id = item.workspace_id;
    rec.user_fields.clone_from(&item.user_fields);
}

fn view(r: &Ranked, version: u64) -> TodoView {
    TodoView {
        id: r.item.id,
        title: r.item.title.clone(),
        kind: r.item.kind,
        owner: r.item.owner,
        status: r.item.status,
        band: r.band,
        importance: r.item.importance,
        urgency: r.item.urgency,
        due_at: r.item.due_at,
        why: r.why.clone(),
        version,
    }
}

/// The ranked queue with explanations.
pub async fn list(app: &App, include_done: bool) -> ApiResult<Vec<TodoView>> {
    let records = app.store.todos(include_done).await?;
    let versions: std::collections::HashMap<TodoId, u64> =
        records.iter().map(|r| (r.id, r.version)).collect();
    let ranked = console_queue::rank(records.iter().map(to_item).collect(), Utc::now());
    Ok(ranked
        .iter()
        .map(|r| view(r, versions.get(&r.item.id).copied().unwrap_or_default()))
        .collect())
}

/// One todo, ranked in the context of the whole queue (so its `why` is accurate).
pub async fn get(app: &App, id: TodoId) -> ApiResult<TodoView> {
    list(app, true)
        .await?
        .into_iter()
        .find(|t| t.id == id)
        .ok_or_else(|| ApiError(RpcError::not_found(format!("todo {id}"))))
}

/// Appends `TodoChanged` for a todo, computing its band with due-date effects.
pub(crate) async fn changed(tx: &mut WriteTx, rec: &TodoRecord) -> Result<(), StoreError> {
    tx.append(
        EventKeys::todo(rec.id),
        EventBody::TodoChanged {
            title: rec.title.clone(),
            band: console_queue::band(&to_item(rec), Utc::now()),
            status: rec.status,
            version: rec.version,
        },
    )
    .await?;
    Ok(())
}

fn proposal_view(p: &ProposalRecord, title: &str) -> ProposalView {
    ProposalView {
        id: p.id,
        todo_id: p.todo_id,
        todo_title: title.to_owned(),
        field: p.field.clone(),
        value: p.value.clone(),
        rationale: p.rationale.clone(),
        expires_at: p.expires_at,
    }
}

fn two() -> Score {
    Score::try_from(2).unwrap_or(Score::MIN)
}

/// Adds a todo. Fields given by the user become theirs; the Orchestrator's choices stay
/// editable by it.
pub async fn add(app: &App, p: TodoAddParams, actor: Actor) -> ApiResult<TodoView> {
    let mut user_fields = Vec::new();
    if actor == Actor::User {
        for (field, given) in [
            ("kind", p.kind.is_some()),
            ("importance", p.importance.is_some()),
            ("urgency", p.urgency.is_some()),
            ("due_at", p.due_at.is_some()),
        ] {
            if given {
                user_fields.push(field.to_owned());
            }
        }
        user_fields.push("title".to_owned());
    }
    let new = NewTodo {
        title: p.title.trim().to_owned(),
        body: p.body,
        kind: p.kind.unwrap_or(TodoKind::Chore),
        owner: if actor == Actor::User {
            TodoOwner::User
        } else {
            TodoOwner::Orchestrator
        },
        status: TodoStatus::Ready,
        importance: p.importance.unwrap_or_else(two),
        urgency: p.urgency.unwrap_or_else(two),
        due_at: p.due_at,
        workspace_id: p.workspace_id,
        user_fields,
    };
    if new.title.is_empty() {
        return Err(ApiError(RpcError::invalid_params("a todo needs a title")));
    }
    let rec = app
        .store
        .write(async |tx| {
            let rec = tx.insert_todo(new).await?;
            changed(tx, &rec).await?;
            Ok(rec)
        })
        .await?;
    get(app, rec.id).await
}

fn changes_from(p: &TodoUpdateParams) -> Vec<Change> {
    let mut c = Vec::new();
    if let Some(v) = &p.title {
        c.push(Change::Title(v.clone()));
    }
    if let Some(v) = p.kind {
        c.push(Change::Kind(v));
    }
    if let Some(v) = p.owner {
        c.push(Change::Owner(v));
    }
    if let Some(v) = p.status {
        c.push(Change::Status(v));
    }
    if let Some(v) = p.importance {
        c.push(Change::Importance(v));
    }
    if let Some(v) = p.urgency {
        c.push(Change::Urgency(v));
    }
    if p.clear_due {
        c.push(Change::DueAt(None));
    } else if let Some(v) = p.due_at {
        c.push(Change::DueAt(Some(v)));
    }
    if let Some(v) = p.workspace_id {
        c.push(Change::Workspace(Some(v)));
    }
    c
}

/// Edits a todo under the ownership rules. `rationale` explains Orchestrator proposals.
pub async fn update(
    app: &App,
    p: TodoUpdateParams,
    actor: Actor,
    rationale: &str,
) -> ApiResult<TodoUpdateResult> {
    let changes = changes_from(&p);
    let rationale = if rationale.is_empty() {
        "suggested by the Orchestrator"
    } else {
        rationale
    };
    let (rec, proposals) = app
        .store
        .write(async |tx| {
            let mut rec = tx.todo(p.todo_id).await?;
            if rec.version != p.expected_version {
                return Err(StoreError::Conflict(format!(
                    "{} is at version {}, not {}",
                    rec.id, rec.version, p.expected_version
                )));
            }
            let edited = console_queue::edit(to_item(&rec), changes, actor);
            apply_item(&mut rec, &edited.item);
            let rec = tx.update_todo(&rec).await?;
            changed(tx, &rec).await?;
            let mut proposals = Vec::new();
            for change in edited.proposed {
                let prop = tx
                    .insert_proposal(
                        rec.id,
                        change.field(),
                        &change.value(),
                        rationale,
                        Utc::now() + Duration::hours(PROPOSAL_TTL_HOURS),
                    )
                    .await?;
                tx.append(
                    EventKeys::todo(rec.id),
                    EventBody::ProposalOpened {
                        proposal_id: prop.id,
                        field: prop.field.clone(),
                        value: prop.value.clone(),
                        rationale: prop.rationale.clone(),
                    },
                )
                .await?;
                proposals.push(prop);
            }
            Ok((rec, proposals))
        })
        .await?;
    Ok(TodoUpdateResult {
        proposals: proposals
            .iter()
            .map(|pr| proposal_view(pr, &rec.title))
            .collect(),
        todo: get(app, rec.id).await?,
    })
}

/// Moves a todo within its band and pins the band's order.
pub async fn move_todo(
    app: &App,
    id: TodoId,
    above_of: Option<TodoId>,
    below_of: Option<TodoId>,
    expected_version: u64,
) -> ApiResult<TodoView> {
    let records = app.store.todos(false).await?;
    let current = records
        .iter()
        .find(|r| r.id == id)
        .ok_or_else(|| ApiError(RpcError::not_found(format!("todo {id}"))))?;
    if current.version != expected_version {
        return Err(ApiError(RpcError::conflict(format!(
            "{id} is at version {}, not {expected_version}",
            current.version
        ))));
    }
    let ranked = console_queue::rank(records.iter().map(to_item).collect(), Utc::now());
    let pins = console_queue::reorder(&ranked, id, above_of, below_of)
        .map_err(|e| ApiError(RpcError::rejected(e.to_string())))?;
    app.store
        .write(async |tx| {
            for (tid, pos) in &pins {
                let mut rec = tx.todo(*tid).await?;
                rec.manual_rank = Some(format!("{pos}"));
                let rec = tx.update_todo(&rec).await?;
                if rec.id == id {
                    changed(tx, &rec).await?;
                }
            }
            Ok(())
        })
        .await?;
    get(app, id).await
}

/// Open proposals, expiring stale ones first.
pub async fn proposals(app: &App) -> ApiResult<Vec<ProposalView>> {
    let now = Utc::now();
    let open = app.store.open_proposals().await?;
    let (expired, live): (Vec<_>, Vec<_>) = open.into_iter().partition(|p| p.expires_at <= now);
    if !expired.is_empty() {
        app.store
            .write(async |tx| {
                for p in &expired {
                    tx.close_proposal(p.id, "expired").await?;
                    tx.append(
                        EventKeys::todo(p.todo_id),
                        EventBody::ProposalClosed {
                            proposal_id: p.id,
                            status: "expired".into(),
                        },
                    )
                    .await?;
                }
                Ok(())
            })
            .await?;
    }
    let mut out = Vec::new();
    for p in live {
        let title = app
            .store
            .todo(p.todo_id)
            .await?
            .map(|t| t.title)
            .unwrap_or_default();
        out.push(proposal_view(&p, &title));
    }
    Ok(out)
}

/// Accepts (applying the change as your own edit) or rejects a proposal.
pub async fn resolve(app: &App, id: ProposalId, accept: bool) -> ApiResult<TodoView> {
    let todo_id = app
        .store
        .write(async |tx| {
            let prop = tx
                .close_proposal(id, if accept { "accepted" } else { "rejected" })
                .await?;
            if accept {
                let change = Change::from_proposal(&prop.field, &prop.value).ok_or_else(|| {
                    StoreError::Invalid(format!("unsupported field {}", prop.field))
                })?;
                let mut rec = tx.todo(prop.todo_id).await?;
                let item = console_queue::accept(to_item(&rec), change);
                apply_item(&mut rec, &item);
                let rec = tx.update_todo(&rec).await?;
                changed(tx, &rec).await?;
            }
            tx.append(
                EventKeys::todo(prop.todo_id),
                EventBody::ProposalClosed {
                    proposal_id: id,
                    status: if accept { "accepted" } else { "rejected" }.into(),
                },
            )
            .await?;
            Ok(prop.todo_id)
        })
        .await?;
    get(app, todo_id).await
}
