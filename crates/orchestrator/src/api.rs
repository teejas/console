//! Client API method handlers (`docs/specs/clients.md`).

use std::sync::Arc;

use console_proto::EventSeq;
use console_proto::api::{
    AttachParams, AttachResult, DecisionRespond, DecisionRespondParams, EVENT_NOTIFICATION, Empty,
    EventsSubscribe, Method, SessionAttach, SessionTarget, SubscribeParams, SubscribeResult,
    TaskCancel, TaskCancelParams, TaskCancelResult, TaskGet, TaskList,
};
use console_proto::domain::SessionKind;
use console_proto::rpc::{EventNotification, Request, RpcError};
use console_store::{EventFilter, StoreError};
use serde_json::Value;
use tokio::sync::broadcast::error::RecvError;

use crate::App;
use crate::engine::{self, EngineError};
use crate::server::Conn;
use crate::task_machine::Input;

/// Events replayed per store read while catching a subscriber up.
const REPLAY_PAGE: u32 = 500;

pub(crate) fn params<M: Method>(params: Value) -> ApiResult<M::Params> {
    let params = if params.is_null() {
        Value::Object(serde_json::Map::new())
    } else {
        params
    };
    serde_json::from_value(params).map_err(|e| ApiError(RpcError::invalid_params(e.to_string())))
}

pub(crate) fn result<M: Method>(r: M::Result) -> ApiResult<Value> {
    serde_json::to_value(r).map_err(|e| ApiError(RpcError::internal(e.to_string())))
}

/// A handler error: an `RpcError` that store and engine errors convert into with `?`.
#[derive(Debug)]
pub struct ApiError(pub RpcError);

pub type ApiResult<T> = Result<T, ApiError>;

impl From<RpcError> for ApiError {
    fn from(e: RpcError) -> Self {
        Self(e)
    }
}

impl From<StoreError> for ApiError {
    fn from(e: StoreError) -> Self {
        Self(match e {
            StoreError::NotFound(m) => RpcError::not_found(m),
            StoreError::Conflict(m) => RpcError::conflict(m),
            StoreError::Invalid(m) => RpcError::invalid_params(m),
            other => RpcError::internal(other.to_string()),
        })
    }
}

impl From<EngineError> for ApiError {
    fn from(e: EngineError) -> Self {
        match e {
            EngineError::Store(s) => s.into(),
            EngineError::Illegal(i) => Self(RpcError::rejected(i.to_string())),
            EngineError::NotFound(t) => Self(RpcError::not_found(format!("task {t}"))),
        }
    }
}

pub async fn dispatch(
    app: &Arc<App>,
    conn: &Conn,
    method: &str,
    p: Value,
) -> Result<Value, RpcError> {
    route(app, conn, method, p).await.map_err(|e| e.0)
}

async fn route(app: &Arc<App>, conn: &Conn, method: &str, p: Value) -> ApiResult<Value> {
    match method {
        SessionAttach::NAME => {
            result::<SessionAttach>(attach(app, params::<SessionAttach>(p)?).await?)
        }
        EventsSubscribe::NAME => {
            result::<EventsSubscribe>(subscribe(app, conn, params::<EventsSubscribe>(p)?).await?)
        }
        TaskGet::NAME => {
            let p = params::<TaskGet>(p)?;
            let task = app
                .store
                .task(p.task_id)
                .await?
                .ok_or_else(|| RpcError::not_found(format!("task {}", p.task_id)))?;
            result::<TaskGet>(task.view())
        }
        TaskList::NAME => {
            let p = params::<TaskList>(p)?;
            let tasks = app.store.tasks(p.include_done).await?;
            result::<TaskList>(tasks.iter().map(console_store::TaskRecord::view).collect())
        }
        TaskCancel::NAME => result::<TaskCancel>(cancel(app, params::<TaskCancel>(p)?).await?),
        DecisionRespond::NAME => {
            result::<DecisionRespond>(respond(app, params::<DecisionRespond>(p)?).await?)
        }
        other => app.dispatch_more(conn, other, p).await,
    }
}

/// The event filter for a session.
pub(crate) async fn filter_for(
    app: &App,
    session_id: console_proto::SessionId,
) -> ApiResult<EventFilter> {
    if session_id == app.orchestrator_session {
        return Ok(EventFilter::Conversation(session_id));
    }
    let session = app
        .store
        .session(session_id)
        .await?
        .ok_or_else(|| ApiError(RpcError::not_found(format!("session {session_id}"))))?;
    match (session.kind, session.task_id) {
        (SessionKind::Worker, Some(task_id)) => Ok(EventFilter::Worker {
            session_id,
            task_id,
        }),
        _ => Ok(EventFilter::Conversation(session_id)),
    }
}

async fn attach(app: &Arc<App>, p: AttachParams) -> ApiResult<AttachResult> {
    let session = match p.target {
        SessionTarget::Orchestrator => app
            .store
            .session(app.orchestrator_session)
            .await?
            .ok_or_else(|| RpcError::internal("orchestrator session missing"))?,
        SessionTarget::Task { task_id } => app
            .store
            .worker_session_for(task_id)
            .await?
            .ok_or_else(|| RpcError::not_found(format!("no worker session for {task_id} yet")))?,
    };
    let filter = filter_for(app, session.id).await?;
    let before = app.store.last_seq().await?;
    let recent = app.store.recent_events(&filter, p.recent).await?;
    let cursor = recent.last().map_or(before, |e| e.seq.max(before));
    let open_decisions = app
        .store
        .open_decisions()
        .await?
        .iter()
        .filter(|d| match filter {
            EventFilter::Worker { task_id, .. } => d.task_id == task_id,
            _ => true,
        })
        .map(console_store::DecisionRecord::view)
        .collect();
    Ok(AttachResult {
        session_id: session.id,
        kind: session.kind,
        title: session.title,
        recent_events: recent,
        open_decisions,
        queue_top: app.queue_top().await?,
        cursor,
    })
}

/// Replays events after the cursor, then streams live ones. Subscribes to the live feed
/// before reading the backlog and drops anything at or below the last sent `seq`, so no
/// event is lost or duplicated in between.
async fn subscribe(app: &Arc<App>, conn: &Conn, p: SubscribeParams) -> ApiResult<SubscribeResult> {
    let filter = filter_for(app, p.session_id).await?;
    let subscription = conn.next_subscription_id();
    let mut live = app.store.subscribe();
    let app2 = app.clone();
    let conn2 = conn.clone();
    let sub = subscription.clone();
    let handle = tokio::spawn(async move {
        let send = |event| {
            let note = Request::notification(
                EVENT_NOTIFICATION,
                serde_json::to_value(EventNotification {
                    subscription: sub.clone(),
                    event,
                })
                .unwrap_or_default(),
            );
            conn2.send(serde_json::to_string(&note).unwrap_or_default())
        };
        let mut last = p.after;
        if !catch_up(&app2, &filter, &mut last, &send).await {
            return;
        }
        loop {
            match live.recv().await {
                Ok(event) => {
                    if event.seq > last && filter.matches(&event) {
                        last = event.seq;
                        if !send(event) {
                            return;
                        }
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    if !catch_up(&app2, &filter, &mut last, &send).await {
                        return;
                    }
                }
                Err(RecvError::Closed) => return,
            }
        }
    });
    conn.track(handle.abort_handle());
    Ok(SubscribeResult { subscription })
}

async fn catch_up(
    app: &App,
    filter: &EventFilter,
    last: &mut EventSeq,
    send: &impl Fn(console_proto::event::Event) -> bool,
) -> bool {
    loop {
        let Ok(page) = app.store.events_after(*last, filter, REPLAY_PAGE).await else {
            return false;
        };
        let full = page.len() == REPLAY_PAGE as usize;
        for event in page {
            *last = event.seq;
            if !send(event) {
                return false;
            }
        }
        if !full {
            return true;
        }
    }
}

async fn cancel(app: &Arc<App>, p: TaskCancelParams) -> ApiResult<TaskCancelResult> {
    let task = engine::apply(&app.store, p.task_id, Input::CancelRequested).await?;
    app.on_cancel_requested(&task).await;
    Ok(TaskCancelResult { state: task.state })
}

async fn respond(app: &Arc<App>, p: DecisionRespondParams) -> ApiResult<Empty> {
    let decision = app
        .store
        .decision(p.decision_id)
        .await?
        .ok_or_else(|| RpcError::not_found(format!("decision {}", p.decision_id)))?;
    if let Some(expected) = p.expected_state {
        let task = app.store.task(decision.task_id).await?;
        if task.map(|t| t.state) != Some(expected) {
            return Err(ApiError(RpcError::conflict(
                "the task moved on; this decision is stale",
            )));
        }
    }
    if !decision.options.is_empty() && !decision.options.iter().any(|o| o.id == p.option_id) {
        return Err(ApiError(RpcError::invalid_params(format!(
            "unknown option {:?}",
            p.option_id
        ))));
    }
    let record = app
        .store
        .write(async |tx| tx.resolve_decision(p.decision_id, &p.option_id, None).await)
        .await?;
    app.on_decision_resolved(&record, None).await;
    Ok(Empty {})
}
