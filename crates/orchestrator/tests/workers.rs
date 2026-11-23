//! Workers end to end over ACP: a fake ACP worker (Python) under a real executor, with the
//! executor's worker tools, decisions, answers, cancel, follow-ups, and patch capture.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use console_agent::Brief;
use console_client::Client;
use console_config::{Config, WorkspaceConfig};
use console_executor::Executor;
use console_orchestrator::{App, server, work};
use console_proto::api::{DecisionRespond, DecisionRespondParams};
use console_proto::domain::{BlockedReason, TaskState};
use console_store::{Store, TaskRecord};

struct Rig {
    _dir: tempfile::TempDir,
    ws: std::path::PathBuf,
    app: Arc<App>,
    client: Client,
}

fn git(dir: &Path, args: &[&str]) {
    assert!(
        std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap()
            .success()
    );
}

async fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("repo-x");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("README.md"), "hi\n").unwrap();
    git(&ws, &["init", "-q", "-b", "main"]);
    git(&ws, &["add", "."]);
    git(&ws, &["commit", "-q", "-m", "init"]);
    let agent = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake_acp_worker.py");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = Config {
        data_dir: dir.path().join("data"),
        ..Config::default()
    };
    config.orchestrator.listen = addr;
    config.executor.orchestrator_url = format!("ws://{addr}/executor");
    config.executor.workspaces = vec![WorkspaceConfig {
        name: "repo-x".into(),
        path: ws.clone(),
    }];
    config.executor.harnesses.insert(
        "fakeacp".into(),
        vec!["python3".into(), agent.display().to_string()],
    );
    config.executor.default_harness = "fakeacp".into();
    let token = config.token().unwrap();
    let app = App::new(
        config.clone(),
        Store::open_in_memory().await.unwrap(),
        token.clone(),
    )
    .await
    .unwrap();
    tokio::spawn(server::serve(app.clone(), listener));
    let exec = Executor::open(config.clone()).await.unwrap();
    exec.start_worker_tools().await.unwrap();
    tokio::spawn(exec.run_until(std::future::pending()));
    for _ in 0..200 {
        if app.store.find_workspace("repo-x").await.unwrap().is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let (client, _) = Client::connect(&config.client_url(), &token).await.unwrap();
    Rig {
        _dir: dir,
        ws,
        app,
        client,
    }
}

fn brief(mode: &str) -> Brief {
    Brief {
        title: format!("Task in {mode} mode"),
        goal: format!("Exercise the worker. MODE:{mode}"),
        workspace: "repo-x".into(),
        context: String::new(),
        constraints: vec![],
        acceptance_criteria: vec!["It works".into()],
        suggested_checks: vec![],
        todo_id: None,
        harness: None,
        importance: None,
        urgency: None,
    }
}

async fn delegate(rig: &Rig, mode: &str) -> TaskRecord {
    let msg = work::delegate(&rig.app, brief(mode), None).await.unwrap();
    assert!(msg.starts_with("Delegated"), "{msg}");
    rig.app
        .store
        .tasks(false)
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.title.contains(mode))
        .unwrap()
}

async fn until(
    rig: &Rig,
    id: console_proto::TaskId,
    what: &str,
    check: impl Fn(&TaskRecord) -> bool,
) -> TaskRecord {
    for _ in 0..400 {
        let t = rig.app.store.task(id).await.unwrap().unwrap();
        if check(&t) {
            return t;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let t = rig.app.store.task(id).await.unwrap().unwrap();
    panic!(
        "timed out waiting for {what}: {:?} {:?} {:?}",
        t.state, t.blocked, t.summary
    );
}

async fn answer(rig: &Rig, option: &str, text: Option<&str>) {
    let d = rig
        .app
        .store
        .open_decisions()
        .await
        .unwrap()
        .pop()
        .expect("an open decision");
    rig.client
        .call::<DecisionRespond>(DecisionRespondParams {
            decision_id: d.id,
            option_id: option.into(),
            expected_state: None,
            answer: text.map(str::to_owned),
        })
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_worker_completes_and_its_patch_is_captured() {
    let rig = rig().await;
    let t = delegate(&rig, "complete").await;
    let t = until(&rig, t.id, "success", |t| t.state == TaskState::Succeeded).await;
    assert!(t.summary.as_deref().unwrap().contains("Wrote result.txt"));
    let patch = rig
        .app
        .store
        .latest_artifact(t.id, "patch")
        .await
        .unwrap()
        .unwrap();
    assert!(patch.content.contains("result.txt"), "{}", patch.content);
    assert!(rig.ws.join("result.txt").exists());
    // Worker output lands in the worker's own session.
    let session = rig
        .app
        .store
        .worker_session_for(t.id)
        .await
        .unwrap()
        .unwrap();
    let events = rig
        .app
        .store
        .events_after(
            console_proto::EventSeq::START,
            &console_store::EventFilter::Worker {
                session_id: session.id,
                task_id: t.id,
            },
            500,
        )
        .await
        .unwrap();
    assert!(events.iter().any(|e| matches!(&e.body, console_proto::event::EventBody::AgentText { text, .. } if text.contains("Working on it"))));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_question_blocks_the_task_until_you_answer() {
    let rig = rig().await;
    let t = delegate(&rig, "ask").await;
    let blocked = until(&rig, t.id, "awaiting input", |t| {
        t.blocked == Some(BlockedReason::AwaitingInput)
    })
    .await;
    assert_eq!(blocked.state, TaskState::Running);
    let todo = rig.app.store.todo(t.todo_id).await.unwrap().unwrap();
    assert_eq!(
        todo.status,
        console_proto::domain::TodoStatus::WaitingOnUser
    );
    answer(&rig, "answer", Some("Use Postgres")).await;
    until(&rig, t.id, "success", |t| t.state == TaskState::Succeeded).await;
    let written = std::fs::read_to_string(rig.ws.join("answer.txt")).unwrap();
    assert!(written.contains("Use Postgres"), "{written}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_permission_request_becomes_a_decision() {
    let rig = rig().await;
    let t = delegate(&rig, "permission").await;
    until(&rig, t.id, "awaiting decision", |t| {
        t.blocked == Some(BlockedReason::AwaitingDecision)
    })
    .await;
    let d = rig.app.store.open_decisions().await.unwrap();
    assert!(d[0].question.contains("git push"));
    assert_eq!(d[0].options.len(), 2);
    answer(&rig, "allow", None).await;
    until(&rig, t.id, "success", |t| t.state == TaskState::Succeeded).await;
    assert!(rig.ws.join("pushed.txt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn silence_is_not_success() {
    let rig = rig().await;
    let t = delegate(&rig, "silent").await;
    until(&rig, t.id, "asked what next", |t| {
        t.blocked == Some(BlockedReason::AwaitingInput)
    })
    .await;
    let d = rig.app.store.open_decisions().await.unwrap();
    assert!(
        d[0].question.contains("without completing"),
        "{}",
        d[0].question
    );
    answer(&rig, "answer", Some("Please finish up.")).await;
    let t = until(&rig, t.id, "success", |t| t.state == TaskState::Succeeded).await;
    assert!(t.summary.unwrap().contains("after a nudge"));
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_stops_a_running_worker() {
    let rig = rig().await;
    let t = delegate(&rig, "slow").await;
    until(&rig, t.id, "running", |t| t.state == TaskState::Running).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    rig.client
        .call::<console_proto::api::TaskCancel>(console_proto::api::TaskCancelParams {
            task_id: t.id,
        })
        .await
        .unwrap();
    until(&rig, t.id, "cancelled", |t| t.state == TaskState::Cancelled).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn follow_ups_land_in_the_queue() {
    let rig = rig().await;
    let t = delegate(&rig, "followup").await;
    until(&rig, t.id, "success", |t| t.state == TaskState::Succeeded).await;
    let todos = rig.app.store.todos(false).await.unwrap();
    assert!(
        todos.iter().any(|t| t.title == "Add more tests"),
        "{:?}",
        todos.iter().map(|t| &t.title).collect::<Vec<_>>()
    );
}
