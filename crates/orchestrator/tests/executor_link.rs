//! The executor link end to end: a real Orchestrator server and a real executor, talking
//! over WebSocket, with a git workspace on disk.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use console_config::{Config, WorkspaceConfig};
use console_executor::Executor;
use console_orchestrator::{App, server};
use console_proto::domain::{Score, TaskState, TodoKind, TodoOwner, TodoStatus};
use console_proto::link::{Op, ReadOp, ReadOpOutcome};
use console_proto::{TaskId, WorkspaceId};
use console_store::{NewTask, NewTodo, Store, TaskRecord};
use tokio::task::JoinHandle;

struct Rig {
    _dir: tempfile::TempDir,
    config: Config,
    app: Arc<App>,
    workspace: PathBuf,
}

fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?}");
}

async fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("repo-x");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::write(
        workspace.join("src/auth.rs"),
        "fn verify_token() {}\n// flaky under clock skew\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("secret.txt"), "outside").unwrap();
    git(&workspace, &["init", "-q", "-b", "main"]);
    git(&workspace, &["add", "."]);
    git(&workspace, &["commit", "-q", "-m", "init"]);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = Config {
        data_dir: dir.path().join("data"),
        ..Config::default()
    };
    config.executor.orchestrator_url = format!("ws://{addr}/executor");
    config.executor.workspaces = vec![WorkspaceConfig {
        name: "repo-x".into(),
        path: workspace.clone(),
    }];
    config.executor.readonly_allowlist = vec![vec!["git".into(), "rev-parse".into()]];

    let token = config.token().unwrap();
    let store = Store::open_in_memory().await.unwrap();
    let app = App::new(config.clone(), store, token).await.unwrap();
    tokio::spawn(server::serve(app.clone(), listener));
    Rig {
        _dir: dir,
        config,
        app,
        workspace,
    }
}

async fn start_executor(config: &Config) -> (Arc<Executor>, JoinHandle<()>) {
    let exec = Executor::open(config.clone()).await.unwrap();
    let e2 = exec.clone();
    let handle = tokio::spawn(async move { e2.run_until(std::future::pending()).await });
    (exec, handle)
}

async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..200 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for: {what}");
}

async fn workspace_id(app: &App) -> WorkspaceId {
    eventually("workspace registered", || async {
        app.store.find_workspace("repo-x").await.unwrap().is_some()
    })
    .await;
    app.store
        .find_workspace("repo-x")
        .await
        .unwrap()
        .unwrap()
        .id
}

async fn task(app: &App, workspace_id: WorkspaceId) -> TaskRecord {
    app.store
        .write(async |tx| {
            let todo = tx
                .insert_todo(NewTodo {
                    title: "Run a command".into(),
                    body: None,
                    kind: TodoKind::Chore,
                    owner: TodoOwner::Orchestrator,
                    status: TodoStatus::Ready,
                    importance: Score::try_from(2).unwrap(),
                    urgency: Score::try_from(2).unwrap(),
                    due_at: None,
                    workspace_id: Some(workspace_id),
                    user_fields: vec![],
                })
                .await?;
            let (t, _) = tx
                .insert_task(NewTask {
                    id: TaskId::generate(),
                    todo_id: todo.id,
                    title: todo.title,
                    workspace_id,
                    harness: "exec".into(),
                    origin_turn_id: None,
                    brief_artifact_id: None,
                    idempotency_key: None,
                })
                .await?;
            Ok(t)
        })
        .await
        .unwrap()
}

async fn state(app: &App, id: TaskId) -> TaskState {
    app.store.task(id).await.unwrap().unwrap().state
}

fn sh(script: &str) -> Vec<String> {
    vec!["sh".into(), "-c".into(), script.into()]
}

#[tokio::test]
async fn registers_workspaces_and_serves_bounded_read_ops() {
    let rig = rig().await;
    let (_exec, _h) = start_executor(&rig.config).await;
    let wid = workspace_id(&rig.app).await;
    let ws = rig.app.store.workspace(wid).await.unwrap().unwrap();
    assert!(ws.head_rev.is_some(), "git head reported");
    assert!(!ws.dirty);

    let read = |op| async {
        rig.app
            .hub
            .read_op(&rig.app, wid, op, Duration::from_secs(5))
            .await
            .unwrap()
    };

    let file = read(ReadOp::ReadFile {
        workspace_id: wid,
        path: "src/auth.rs".into(),
        max_bytes: 10,
    })
    .await;
    assert_eq!(
        file,
        ReadOpOutcome::Ok {
            output: "fn verify_".into(),
            truncated: true
        }
    );

    let dir = read(ReadOp::ListDir {
        workspace_id: wid,
        path: String::new(),
    })
    .await;
    assert!(
        matches!(&dir, ReadOpOutcome::Ok { output, .. } if output.contains("src/")),
        "{dir:?}"
    );

    let found = read(ReadOp::Search {
        workspace_id: wid,
        pattern: "flaky".into(),
        path_glob: None,
        max_matches: 10,
    })
    .await;
    assert!(
        matches!(&found, ReadOpOutcome::Ok { output, .. } if output.starts_with("src/auth.rs:2:")),
        "{found:?}"
    );

    let status = read(ReadOp::GitStatus { workspace_id: wid }).await;
    assert!(
        matches!(&status, ReadOpOutcome::Ok { output, .. } if output.contains("main")),
        "{status:?}"
    );

    let escape = read(ReadOp::ReadFile {
        workspace_id: wid,
        path: "../secret.txt".into(),
        max_bytes: 100,
    })
    .await;
    assert!(matches!(escape, ReadOpOutcome::Denied { .. }), "{escape:?}");

    let not_allowed = read(ReadOp::RunReadonly {
        workspace_id: wid,
        argv: vec!["touch".into(), "x".into()],
    })
    .await;
    assert!(matches!(not_allowed, ReadOpOutcome::Denied { .. }));
    assert!(!rig.workspace.join("x").exists(), "nothing ran");

    let allowed = read(ReadOp::RunReadonly {
        workspace_id: wid,
        argv: vec![
            "git".into(),
            "rev-parse".into(),
            "--abbrev-ref".into(),
            "HEAD".into(),
        ],
    })
    .await;
    assert_eq!(
        allowed,
        ReadOpOutcome::Ok {
            output: "main\n".into(),
            truncated: false
        }
    );

    let bad_rev = read(ReadOp::GitShow {
        workspace_id: wid,
        rev: "--output=/tmp/x".into(),
    })
    .await;
    assert!(matches!(bad_rev, ReadOpOutcome::Denied { .. }));
}

#[tokio::test]
async fn a_dispatched_command_runs_once_and_reports() {
    let rig = rig().await;
    let (_exec, _h) = start_executor(&rig.config).await;
    let wid = workspace_id(&rig.app).await;
    let t = task(&rig.app, wid).await;

    let cmd = rig
        .app
        .hub
        .start(
            &rig.app,
            &t,
            Op::Exec {
                workspace_id: wid,
                argv: sh("echo hi"),
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();
    eventually("task succeeded", || async {
        state(&rig.app, t.id).await == TaskState::Succeeded
    })
    .await;
    let done = rig.app.store.task(t.id).await.unwrap().unwrap();
    assert_eq!(done.summary.as_deref(), Some("hi\n"));
    let todo = rig.app.store.todo(t.todo_id).await.unwrap().unwrap();
    assert_eq!(todo.status, TodoStatus::Done, "the todo follows its task");

    // A redelivered dispatch is answered from the journal, not run again.
    assert!(rig.app.hub.send_to(
        cmd.executor_id,
        &console_proto::link::OrchestratorMsg::Dispatch {
            command_id: cmd.id,
            attempt_id: cmd.attempt_id.unwrap(),
            epoch: console_orchestrator::hub::EPOCH,
            op: cmd.op.clone(),
        }
    ));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(state(&rig.app, t.id).await, TaskState::Succeeded);
}

#[tokio::test]
async fn cancel_stops_the_command() {
    let rig = rig().await;
    let (_exec, _h) = start_executor(&rig.config).await;
    let wid = workspace_id(&rig.app).await;
    let t = task(&rig.app, wid).await;
    rig.app
        .hub
        .start(
            &rig.app,
            &t,
            Op::Exec {
                workspace_id: wid,
                argv: sh("sleep 30"),
                timeout_ms: 60_000,
            },
        )
        .await
        .unwrap();
    eventually("running", || async {
        state(&rig.app, t.id).await == TaskState::Running
    })
    .await;

    let (client, _) = console_client::Client::connect(
        &rig.config
            .executor
            .orchestrator_url
            .replace("/executor", "/client"),
        &rig.config.token().unwrap(),
    )
    .await
    .unwrap();
    let r = client
        .call::<console_proto::api::TaskCancel>(console_proto::api::TaskCancelParams {
            task_id: t.id,
        })
        .await
        .unwrap();
    assert_eq!(r.state, TaskState::CancelRequested);
    eventually("cancel confirmed", || async {
        state(&rig.app, t.id).await == TaskState::Cancelled
    })
    .await;
}

#[tokio::test]
async fn executor_crash_mid_command_becomes_outcome_unknown_and_never_reruns() {
    let rig = rig().await;
    let marker = rig.workspace.join("runs.txt");
    let (exec, handle) = start_executor(&rig.config).await;
    let wid = workspace_id(&rig.app).await;
    let t = task(&rig.app, wid).await;
    rig.app
        .hub
        .start(
            &rig.app,
            &t,
            Op::Exec {
                workspace_id: wid,
                argv: sh("echo run >> runs.txt; sleep 30"),
                timeout_ms: 60_000,
            },
        )
        .await
        .unwrap();
    eventually("running", || async {
        state(&rig.app, t.id).await == TaskState::Running
    })
    .await;
    eventually("command started", || async { marker.exists() }).await;

    // Crash: drop the connection and every in-flight command, recording nothing.
    exec.abort_all();
    handle.abort();
    eventually("executor offline", || async {
        rig.app.store.task(t.id).await.unwrap().unwrap().blocked
            == Some(console_proto::domain::BlockedReason::WaitingExecutor)
    })
    .await;

    // Restart with the same journal.
    let (_exec2, _h2) = start_executor(&rig.config).await;
    eventually("outcome unknown", || async {
        state(&rig.app, t.id).await == TaskState::OutcomeUnknown
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let runs = std::fs::read_to_string(&marker).unwrap();
    assert_eq!(runs.lines().count(), 1, "the command was not re-run");
    let todo = rig.app.store.todo(t.todo_id).await.unwrap().unwrap();
    assert_eq!(
        todo.status,
        TodoStatus::WaitingOnUser,
        "an unknown outcome needs you"
    );
}

#[tokio::test]
async fn dispatch_while_offline_waits_and_runs_on_connect() {
    let rig = rig().await;
    // Register the workspace, then go offline.
    let (exec, handle) = start_executor(&rig.config).await;
    let wid = workspace_id(&rig.app).await;
    exec.abort_all();
    handle.abort();
    eventually("offline", || async {
        !rig.app
            .store
            .executors()
            .await
            .unwrap()
            .iter()
            .any(|e| e.online)
    })
    .await;

    let t = task(&rig.app, wid).await;
    rig.app
        .hub
        .start(
            &rig.app,
            &t,
            Op::Exec {
                workspace_id: wid,
                argv: sh("echo later"),
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();
    let waiting = rig.app.store.task(t.id).await.unwrap().unwrap();
    assert_eq!(
        (waiting.state, waiting.blocked),
        (
            TaskState::Queued,
            Some(console_proto::domain::BlockedReason::WaitingExecutor)
        )
    );

    let (_exec2, _h2) = start_executor(&rig.config).await;
    eventually("ran after reconnect", || async {
        state(&rig.app, t.id).await == TaskState::Succeeded
    })
    .await;
}
