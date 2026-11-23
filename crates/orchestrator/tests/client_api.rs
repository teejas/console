//! The client API end to end: a real server, a real WebSocket client.

use std::sync::Arc;
use std::time::Duration;

use console_client::{Client, ClientError};
use console_config::Config;
use console_orchestrator::task_machine::Input;
use console_orchestrator::{App, engine, server};
use console_proto::api::{
    AttachParams, DecisionRespond, DecisionRespondParams, EventsSubscribe, SessionAttach,
    SessionTarget, SubscribeParams, TaskCancel, TaskCancelParams, TaskGet, TaskGetParams, TaskList,
    TaskListParams,
};
use console_proto::domain::{Score, TaskState, TodoKind, TodoOwner, TodoStatus};
use console_proto::event::{DecisionOption, EventBody};
use console_proto::rpc::{EventNotification, RpcError};
use console_proto::{DecisionId, EventSeq, TaskId, WorkspaceId};
use console_store::{DecisionOrigin, NewTask, NewTodo, Store, TaskRecord};
use tokio::sync::mpsc::UnboundedReceiver;

const TOKEN: &str = "test-token";

async fn start() -> (Arc<App>, String) {
    let store = Store::open_in_memory().await.unwrap();
    let app = App::new(Config::default(), store, TOKEN.into())
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/client", listener.local_addr().unwrap());
    tokio::spawn(server::serve(app.clone(), listener));
    (app, url)
}

async fn task_with_todo(app: &App) -> TaskRecord {
    app.store
        .write(async |tx| {
            let todo = tx
                .insert_todo(NewTodo {
                    title: "Fix flaky auth test".into(),
                    body: None,
                    kind: TodoKind::Bugfix,
                    owner: TodoOwner::Orchestrator,
                    status: TodoStatus::Ready,
                    importance: Score::try_from(3).unwrap(),
                    urgency: Score::try_from(2).unwrap(),
                    due_at: None,
                    workspace_id: None,
                    user_fields: vec![],
                })
                .await?;
            let (task, _) = tx
                .insert_task(NewTask {
                    id: TaskId::generate(),
                    todo_id: todo.id,
                    title: todo.title.clone(),
                    workspace_id: WorkspaceId::generate(),
                    harness: "claude".into(),
                    origin_turn_id: None,
                    brief_artifact_id: None,
                    idempotency_key: None,
                })
                .await?;
            Ok(task)
        })
        .await
        .unwrap()
}

async fn next(rx: &mut UnboundedReceiver<EventNotification>) -> EventBody {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("event within 5s")
        .expect("stream open")
        .event
        .body
}

#[tokio::test]
async fn rejects_a_bad_token() {
    let (_app, url) = start().await;
    let err = Client::connect(&url, "wrong").await.unwrap_err();
    assert!(matches!(err, ClientError::Connect { .. }), "{err}");
}

#[tokio::test]
async fn attach_subscribe_and_follow_a_task() {
    let (app, url) = start().await;
    let (client, mut events) = Client::connect(&url, TOKEN).await.unwrap();

    let attached = client
        .call::<SessionAttach>(AttachParams {
            target: SessionTarget::Orchestrator,
            recent: 50,
        })
        .await
        .unwrap();
    assert_eq!(attached.session_id, app.orchestrator_session);
    client
        .call::<EventsSubscribe>(SubscribeParams {
            session_id: attached.session_id,
            after: attached.cursor,
        })
        .await
        .unwrap();

    let task = task_with_todo(&app).await;
    engine::apply(&app.store, task.id, Input::Accepted)
        .await
        .unwrap();
    assert!(matches!(
        next(&mut events).await,
        EventBody::TaskTransition {
            to: TaskState::Running,
            ..
        }
    ));
    assert!(
        matches!(
            next(&mut events).await,
            EventBody::TodoChanged {
                status: TodoStatus::Active,
                ..
            }
        ),
        "the todo follows its task"
    );

    let got = client
        .call::<TaskGet>(TaskGetParams { task_id: task.id })
        .await
        .unwrap();
    assert_eq!(got.state, TaskState::Running);
    assert_eq!(
        client
            .call::<TaskList>(TaskListParams::default())
            .await
            .unwrap()
            .len(),
        1
    );

    let cancelled = client
        .call::<TaskCancel>(TaskCancelParams { task_id: task.id })
        .await
        .unwrap();
    assert_eq!(
        cancelled.state,
        TaskState::CancelRequested,
        "cancel is a request until confirmed"
    );
    engine::apply(&app.store, task.id, Input::CancelConfirmed)
        .await
        .unwrap();
    let mut saw_cancelled = false;
    for _ in 0..4 {
        if matches!(
            next(&mut events).await,
            EventBody::TaskTransition {
                to: TaskState::Cancelled,
                ..
            }
        ) {
            saw_cancelled = true;
            break;
        }
    }
    assert!(saw_cancelled);

    // Terminal tasks reject further moves.
    let err = client
        .call::<TaskCancel>(TaskCancelParams { task_id: task.id })
        .await;
    assert!(
        err.is_ok(),
        "cancelling a cancelled task is a harmless no-op"
    );
    assert!(
        client
            .call::<TaskList>(TaskListParams::default())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn reconnect_replays_exactly_what_was_missed() {
    let (app, url) = start().await;
    let task = task_with_todo(&app).await;
    engine::apply(&app.store, task.id, Input::Accepted)
        .await
        .unwrap();

    // First client sees everything so far, remembers its cursor, and disconnects.
    let cursor = {
        let (client, _) = Client::connect(&url, TOKEN).await.unwrap();
        let a = client
            .call::<SessionAttach>(AttachParams {
                target: SessionTarget::Orchestrator,
                recent: 50,
            })
            .await
            .unwrap();
        assert!(!a.recent_events.is_empty());
        a.cursor
    };

    // Work continues while nobody is connected.
    engine::apply(&app.store, task.id, Input::Succeeded)
        .await
        .unwrap();

    let (client, mut events) = Client::connect(&url, TOKEN).await.unwrap();
    client
        .call::<EventsSubscribe>(SubscribeParams {
            session_id: app.orchestrator_session,
            after: cursor,
        })
        .await
        .unwrap();
    assert!(matches!(
        next(&mut events).await,
        EventBody::TaskTransition {
            to: TaskState::Succeeded,
            ..
        }
    ));
    assert!(matches!(
        next(&mut events).await,
        EventBody::TodoChanged {
            status: TodoStatus::Done,
            ..
        }
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(200), events.recv())
            .await
            .is_err(),
        "nothing before the cursor is replayed"
    );

    // From the very start, everything is replayed in order.
    client
        .call::<EventsSubscribe>(SubscribeParams {
            session_id: app.orchestrator_session,
            after: EventSeq::START,
        })
        .await
        .unwrap();
    let mut seqs = Vec::new();
    while let Ok(Some(n)) = tokio::time::timeout(Duration::from_millis(200), events.recv()).await {
        seqs.push(n.event.seq);
    }
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "in order: {seqs:?}");
    assert_eq!(seqs.len(), 4);
}

#[tokio::test]
async fn decisions_resolve_once() {
    let (app, url) = start().await;
    let (client, _) = Client::connect(&url, TOKEN).await.unwrap();
    let task = task_with_todo(&app).await;
    let did = DecisionId::generate();
    app.store
        .write(async |tx| {
            tx.insert_decision(
                did,
                task.id,
                DecisionOrigin::Permission,
                "Allow `git push`?",
                vec![
                    DecisionOption {
                        id: "allow".into(),
                        label: "Allow".into(),
                    },
                    DecisionOption {
                        id: "deny".into(),
                        label: "Deny".into(),
                    },
                ],
            )
            .await
        })
        .await
        .unwrap();

    let attached = client
        .call::<SessionAttach>(AttachParams {
            target: SessionTarget::Orchestrator,
            recent: 10,
        })
        .await
        .unwrap();
    assert_eq!(attached.open_decisions.len(), 1);

    let bad = client
        .call::<DecisionRespond>(DecisionRespondParams {
            decision_id: did,
            option_id: "maybe".into(),
            expected_state: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(
        bad,
        ClientError::Rpc(RpcError {
            code: RpcError::INVALID_PARAMS,
            ..
        })
    ));

    client
        .call::<DecisionRespond>(DecisionRespondParams {
            decision_id: did,
            option_id: "allow".into(),
            expected_state: None,
        })
        .await
        .unwrap();
    let again = client
        .call::<DecisionRespond>(DecisionRespondParams {
            decision_id: did,
            option_id: "deny".into(),
            expected_state: None,
        })
        .await
        .unwrap_err();
    assert!(
        matches!(
            again,
            ClientError::Rpc(RpcError {
                code: RpcError::CONFLICT,
                ..
            })
        ),
        "{again}"
    );
}

#[tokio::test]
async fn unknown_methods_are_reported() {
    let (_app, url) = start().await;
    let (client, _) = Client::connect(&url, TOKEN).await.unwrap();
    let err = client
        .call_raw("nope.nothing", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        ClientError::Rpc(RpcError {
            code: RpcError::METHOD_NOT_FOUND,
            ..
        })
    ));
}
