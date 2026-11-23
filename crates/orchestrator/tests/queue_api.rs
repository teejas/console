//! The shared queue over the client API, plus the Orchestrator's proposal path.

use std::sync::Arc;

use console_client::{Client, ClientError};
use console_config::Config;
use console_orchestrator::{App, queue_ops, server};
use console_proto::api::{
    AttachParams, Empty, ProposalList, ProposalResolve, ProposalResolveParams, QueueList,
    QueueListParams, SessionAttach, SessionTarget, TodoAdd, TodoAddParams, TodoMove,
    TodoMoveParams, TodoUpdate, TodoUpdateParams, TodoView,
};
use console_proto::domain::{Band, Score, TodoStatus};
use console_proto::rpc::RpcError;
use console_queue::Actor;
use console_store::Store;

async fn start() -> (Arc<App>, Client) {
    let store = Store::open_in_memory().await.unwrap();
    let app = App::new(Config::default(), store, "t".into())
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/client", listener.local_addr().unwrap());
    tokio::spawn(server::serve(app.clone(), listener));
    let (client, _) = Client::connect(&url, "t").await.unwrap();
    (app, client)
}

fn s(v: u8) -> Score {
    Score::try_from(v).unwrap()
}

fn add(title: &str, importance: u8, urgency: u8) -> TodoAddParams {
    TodoAddParams {
        title: title.into(),
        body: None,
        kind: None,
        importance: Some(s(importance)),
        urgency: Some(s(urgency)),
        due_at: None,
        workspace_id: None,
    }
}

fn update(t: &TodoView) -> TodoUpdateParams {
    TodoUpdateParams {
        todo_id: t.id,
        expected_version: t.version,
        title: None,
        kind: None,
        owner: None,
        status: None,
        importance: None,
        urgency: None,
        due_at: None,
        clear_due: false,
        workspace_id: None,
    }
}

async fn titles(client: &Client) -> Vec<String> {
    client
        .call::<QueueList>(QueueListParams::default())
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.title)
        .collect()
}

#[tokio::test]
async fn add_rank_move_and_finish() {
    let (_app, client) = start().await;
    let chore = client
        .call::<TodoAdd>(add("bump tokio", 1, 1))
        .await
        .unwrap();
    let review = client
        .call::<TodoAdd>(add("review PR 42", 2, 4))
        .await
        .unwrap();
    let feature = client
        .call::<TodoAdd>(add("auth refactor", 4, 2))
        .await
        .unwrap();
    let fire = client
        .call::<TodoAdd>(add("main is red", 4, 4))
        .await
        .unwrap();
    assert_eq!(fire.band, Band::P0);
    assert_eq!(
        titles(&client).await,
        ["main is red", "review PR 42", "auth refactor", "bump tokio"]
    );

    let q = client
        .call::<QueueList>(QueueListParams::default())
        .await
        .unwrap();
    assert!(q[1].why.starts_with("P1"), "{}", q[1].why);

    // Moving across bands is refused with a reason.
    let err = client
        .call::<TodoMove>(TodoMoveParams {
            todo_id: chore.id,
            before: Some(feature.id),
            after: None,
            expected_version: chore.version,
        })
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            ClientError::Rpc(RpcError {
                code: RpcError::REJECTED,
                ..
            })
        ),
        "{err}"
    );

    // Within a band, a move sticks.
    let other = client
        .call::<TodoAdd>(add("write design doc", 3, 1))
        .await
        .unwrap();
    assert_eq!(
        titles(&client).await[2..4],
        ["auth refactor".to_string(), "write design doc".to_string()]
    );
    let moved = client
        .call::<TodoMove>(TodoMoveParams {
            todo_id: other.id,
            before: Some(feature.id),
            after: None,
            expected_version: other.version,
        })
        .await
        .unwrap();
    assert!(moved.why.contains("placed by you"));
    assert_eq!(
        titles(&client).await[2..4],
        ["write design doc".to_string(), "auth refactor".to_string()]
    );

    // Stale versions conflict.
    let err = client.call::<TodoUpdate>(update(&review)).await;
    assert!(err.is_ok(), "fresh version applies");
    let err = client
        .call::<TodoUpdate>(update(&review))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        ClientError::Rpc(RpcError {
            code: RpcError::CONFLICT,
            ..
        })
    ));

    // Done items leave the open queue.
    let q = client
        .call::<QueueList>(QueueListParams::default())
        .await
        .unwrap();
    let fire_now = q.iter().find(|t| t.id == fire.id).unwrap();
    let mut done = update(fire_now);
    done.status = Some(TodoStatus::Done);
    client.call::<TodoUpdate>(done).await.unwrap();
    assert!(!titles(&client).await.contains(&"main is red".to_string()));
    assert_eq!(
        client
            .call::<QueueList>(QueueListParams { include_done: true })
            .await
            .unwrap()
            .len(),
        5
    );
}

#[tokio::test]
async fn orchestrator_edits_to_your_fields_become_proposals() {
    let (app, client) = start().await;
    let todo = client
        .call::<TodoAdd>(add("ship the MVP", 4, 2))
        .await
        .unwrap();

    // The Orchestrator tries to lower importance (yours) and set a workspace (shared).
    let mut p = update(&todo);
    p.importance = Some(s(1));
    p.status = Some(TodoStatus::Active);
    let r = queue_ops::update(&app, p, Actor::Orchestrator, "nothing depends on it yet")
        .await
        .unwrap();
    assert_eq!(r.todo.importance, s(4), "your value is kept");
    assert_eq!(
        r.todo.status,
        TodoStatus::Active,
        "workflow status is shared"
    );
    assert_eq!(r.proposals.len(), 1);
    assert_eq!(r.proposals[0].rationale, "nothing depends on it yet");

    let open = client.call::<ProposalList>(Empty {}).await.unwrap();
    assert_eq!(open.len(), 1);
    let accepted = client
        .call::<ProposalResolve>(ProposalResolveParams {
            proposal_id: open[0].id,
            accept: true,
        })
        .await
        .unwrap();
    assert_eq!(accepted.importance, s(1));
    assert!(
        client
            .call::<ProposalList>(Empty {})
            .await
            .unwrap()
            .is_empty()
    );

    // A resolved proposal can't be resolved again.
    let again = client
        .call::<ProposalResolve>(ProposalResolveParams {
            proposal_id: open[0].id,
            accept: false,
        })
        .await
        .unwrap_err();
    assert!(matches!(
        again,
        ClientError::Rpc(RpcError {
            code: RpcError::CONFLICT,
            ..
        })
    ));

    // Attach snapshots include the queue top.
    let a = client
        .call::<SessionAttach>(AttachParams {
            target: SessionTarget::Orchestrator,
            recent: 10,
        })
        .await
        .unwrap();
    assert_eq!(a.queue_top.len(), 1);
}
