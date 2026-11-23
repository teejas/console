//! Store behavior: the outbox rule, compare-and-set transitions, rollback, and replay.

use console_proto::domain::{BlockedReason, SessionKind, TaskState};
use console_proto::event::EventBody;
use console_proto::{EventSeq, TaskId, TodoId, WorkspaceId};
use console_store::{EventFilter, EventKeys, NewTask, Store, StoreError};

fn new_task(key: Option<&str>) -> NewTask {
    NewTask {
        id: TaskId::generate(),
        todo_id: TodoId::generate(),
        title: "Fix flaky test".into(),
        workspace_id: WorkspaceId::generate(),
        harness: "claude".into(),
        origin_turn_id: None,
        brief_artifact_id: None,
        idempotency_key: key.map(str::to_owned),
    }
}

#[tokio::test]
async fn transition_appends_event_and_broadcasts_after_commit() {
    let store = Store::open_in_memory().await.unwrap();
    let mut rx = store.subscribe();
    let (task, created) = store
        .write(async |tx| tx.insert_task(new_task(None)).await)
        .await
        .unwrap();
    assert!(created);
    assert_eq!(task.state, TaskState::Queued);

    let moved = store
        .write(async |tx| {
            tx.transition_task(
                task.id,
                TaskState::Queued,
                task.version,
                TaskState::Running,
                None,
            )
            .await
        })
        .await
        .unwrap();
    assert_eq!(moved.state, TaskState::Running);
    assert_eq!(moved.version, task.version + 1);

    let event = rx.recv().await.unwrap();
    assert_eq!(event.task_id, Some(task.id));
    assert!(matches!(
        event.body,
        EventBody::TaskTransition {
            from: TaskState::Queued,
            to: TaskState::Running,
            ..
        }
    ));
    let stored = store
        .events_after(EventSeq::START, &EventFilter::All, 10)
        .await
        .unwrap();
    assert_eq!(stored, vec![event]);
}

#[tokio::test]
async fn stale_transition_conflicts_and_changes_nothing() {
    let store = Store::open_in_memory().await.unwrap();
    let (task, _) = store
        .write(async |tx| tx.insert_task(new_task(None)).await)
        .await
        .unwrap();
    store
        .write(async |tx| {
            tx.transition_task(
                task.id,
                TaskState::Queued,
                task.version,
                TaskState::Running,
                None,
            )
            .await
        })
        .await
        .unwrap();
    // A second writer still holding the old version loses.
    let err = store
        .write(async |tx| {
            tx.transition_task(
                task.id,
                TaskState::Queued,
                task.version,
                TaskState::Cancelled,
                None,
            )
            .await
        })
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Conflict(_)), "{err}");
    let now = store.task(task.id).await.unwrap().unwrap();
    assert_eq!(now.state, TaskState::Running);
    let events = store
        .events_after(EventSeq::START, &EventFilter::All, 10)
        .await
        .unwrap();
    assert_eq!(events.len(), 1, "only the winning transition is logged");
}

#[tokio::test]
async fn failed_write_rolls_back_and_publishes_nothing() {
    let store = Store::open_in_memory().await.unwrap();
    let mut rx = store.subscribe();
    let task_id = TaskId::generate();
    let result: Result<(), StoreError> = store
        .write(async |tx| {
            tx.append(
                EventKeys::task(task_id),
                EventBody::TaskTransition {
                    from: TaskState::Queued,
                    to: TaskState::Running,
                    blocked: Some(BlockedReason::WaitingExecutor),
                },
            )
            .await?;
            Err(StoreError::Invalid("boom".into()))
        })
        .await;
    assert!(result.is_err());
    assert!(rx.try_recv().is_err(), "nothing broadcast");
    assert_eq!(
        store.last_seq().await.unwrap(),
        EventSeq::START,
        "nothing stored"
    );
}

#[tokio::test]
async fn idempotency_key_returns_the_existing_task() {
    let store = Store::open_in_memory().await.unwrap();
    let (a, created_a) = store
        .write(async |tx| tx.insert_task(new_task(Some("k1"))).await)
        .await
        .unwrap();
    let (b, created_b) = store
        .write(async |tx| tx.insert_task(new_task(Some("k1"))).await)
        .await
        .unwrap();
    assert!(created_a && !created_b);
    assert_eq!(a.id, b.id);
}

#[tokio::test]
async fn cursor_replay_and_filters() {
    let store = Store::open_in_memory().await.unwrap();
    let orch = store.orchestrator_session().await.unwrap();
    assert_eq!(orch.kind, SessionKind::Orchestrator);
    assert_eq!(
        store.orchestrator_session().await.unwrap().id,
        orch.id,
        "created once"
    );

    let (worker_task, _) = store
        .write(async |tx| tx.insert_task(new_task(None)).await)
        .await
        .unwrap();
    let worker = store
        .write(async |tx| {
            tx.insert_session(SessionKind::Worker, Some(worker_task.id), "w")
                .await
        })
        .await
        .unwrap();

    store
        .write(async |tx| {
            tx.append(
                EventKeys::session(orch.id),
                EventBody::AgentText {
                    turn_id: None,
                    text: "a".into(),
                },
            )
            .await?;
            tx.append(
                EventKeys::session(worker.id),
                EventBody::AgentText {
                    turn_id: None,
                    text: "w1".into(),
                },
            )
            .await?;
            tx.append(
                EventKeys::task(worker_task.id),
                EventBody::TaskTransition {
                    from: TaskState::Queued,
                    to: TaskState::Running,
                    blocked: None,
                },
            )
            .await?;
            tx.append(
                EventKeys::session(orch.id),
                EventBody::AgentText {
                    turn_id: None,
                    text: "b".into(),
                },
            )
            .await
        })
        .await
        .unwrap();

    let conversation = EventFilter::Conversation(orch.id);
    let all = store
        .events_after(EventSeq::START, &conversation, 100)
        .await
        .unwrap();
    // Orchestrator text and the unscoped task transition, but not the worker's own text.
    assert_eq!(all.len(), 3);
    assert!(all.iter().all(|e| conversation.matches(e)));

    let after_first = store
        .events_after(all[0].seq, &conversation, 100)
        .await
        .unwrap();
    assert_eq!(after_first, all[1..].to_vec());

    let recent = store.recent_events(&conversation, 2).await.unwrap();
    assert_eq!(recent, all[1..].to_vec(), "most recent, oldest first");

    let worker_filter = EventFilter::Worker {
        session_id: worker.id,
        task_id: worker_task.id,
    };
    let worker_events = store
        .events_after(EventSeq::START, &worker_filter, 100)
        .await
        .unwrap();
    assert_eq!(worker_events.len(), 2);
}

#[tokio::test]
async fn file_store_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("o.db");
    let task_id = {
        let store = Store::open(&path).await.unwrap();
        let (task, _) = store
            .write(async |tx| tx.insert_task(new_task(None)).await)
            .await
            .unwrap();
        task.id
    };
    let store = Store::open(&path).await.unwrap();
    assert_eq!(
        store.task(task_id).await.unwrap().unwrap().state,
        TaskState::Queued
    );
}
