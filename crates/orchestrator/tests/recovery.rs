//! Failure and recovery end to end: the Orchestrator crashing mid-task, the executor
//! crashing mid-worker, and settling an unknown outcome by retry or by hand.
//!
//! The Orchestrator runs on its own runtime so a crash can drop every connection at once.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use console_agent::Brief;
use console_client::Client;
use console_config::{Config, WorkspaceConfig};
use console_executor::Executor;
use console_orchestrator::{App, server, work};
use console_proto::TaskId;
use console_proto::api::{
    AttachParams, DecisionRespond, DecisionRespondParams, ResolvedOutcome, SessionAttach,
    SessionTarget, TaskGet, TaskGetParams, TaskResolve, TaskResolveParams, TaskRetry, TaskView,
};
use console_proto::domain::{BlockedReason, TaskState};
use console_store::Store;
use tokio::task::JoinHandle;

struct Rig {
    _dir: tempfile::TempDir,
    config: Config,
    db: PathBuf,
    addr: SocketAddr,
    ws: PathBuf,
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

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("repo-x");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("README.md"), "hi\n").unwrap();
    git(&ws, &["init", "-q", "-b", "main"]);
    git(&ws, &["add", "."]);
    git(&ws, &["commit", "-q", "-m", "init"]);
    let addr = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let agent = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fake_acp_worker.py");
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
    let db = dir.path().join("orchestrator.db");
    Rig {
        _dir: dir,
        config,
        db,
        addr,
        ws,
    }
}

/// An Orchestrator on its own runtime. Dropping it with `crash` kills every connection.
struct Orch {
    rt: Option<tokio::runtime::Runtime>,
    app: Arc<App>,
}

impl Orch {
    async fn start(rig: &Rig) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (config, db, addr) = (rig.config.clone(), rig.db.clone(), rig.addr);
        let app = rt
            .spawn(async move {
                let token = config.token().unwrap();
                let store = Store::open(&db).await.unwrap();
                let app = App::new(config, store, token).await.unwrap();
                let listener = loop {
                    match tokio::net::TcpListener::bind(addr).await {
                        Ok(l) => break l,
                        Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
                    }
                };
                tokio::spawn(server::serve(app.clone(), listener));
                app
            })
            .await
            .unwrap();
        Self { rt: Some(rt), app }
    }

    fn crash(mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }

    async fn delegate(&self, mode: &str) -> TaskId {
        let app = self.app.clone();
        let brief = Brief {
            title: format!("Task in {mode} mode"),
            goal: format!("MODE:{mode}"),
            workspace: "repo-x".into(),
            context: String::new(),
            constraints: vec![],
            acceptance_criteria: vec![],
            suggested_checks: vec![],
            todo_id: None,
            harness: None,
            importance: None,
            urgency: None,
        };
        let rt = self.rt.as_ref().unwrap();
        rt.spawn(async move {
            for _ in 0..200 {
                if app.store.find_workspace("repo-x").await.unwrap().is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            work::delegate(&app, brief, None).await.unwrap();
            app.store.tasks(true).await.unwrap()[0].id
        })
        .await
        .unwrap()
    }
}

impl Drop for Orch {
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

async fn start_executor(config: &Config) -> (Arc<Executor>, JoinHandle<()>) {
    let exec = Executor::open(config.clone()).await.unwrap();
    exec.start_worker_tools().await.unwrap();
    let e2 = exec.clone();
    (
        exec,
        tokio::spawn(async move { e2.run_until(std::future::pending()).await }),
    )
}

async fn client(rig: &Rig) -> Client {
    for _ in 0..200 {
        if let Ok((c, _)) =
            Client::connect(&rig.config.client_url(), &rig.config.token().unwrap()).await
        {
            return c;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("no Orchestrator");
}

async fn until(c: &Client, id: TaskId, what: &str, check: impl Fn(&TaskView) -> bool) -> TaskView {
    let mut last = None;
    for _ in 0..400 {
        if let Ok(t) = c.call::<TaskGet>(TaskGetParams { task_id: id }).await {
            if check(&t) {
                return t;
            }
            last = Some(t);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for {what}: {last:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn work_survives_an_orchestrator_crash() {
    let rig = rig();
    let orch = Orch::start(&rig).await;
    let (_exec, _h) = start_executor(&rig.config).await;
    let id = orch.delegate("ask").await;
    let c = client(&rig).await;
    until(&c, id, "a question", |t| {
        t.blocked == Some(BlockedReason::AwaitingInput)
    })
    .await;

    // The Orchestrator dies while the worker waits for an answer.
    drop(c);
    orch.crash();
    let _orch = Orch::start(&rig).await;
    let c = client(&rig).await;

    // The executor reconnects on its own; the question is still open, and answering it
    // reaches the same worker.
    let decisions = c
        .call::<SessionAttach>(AttachParams {
            target: SessionTarget::Orchestrator,
            recent: 0,
        })
        .await
        .unwrap()
        .open_decisions;
    assert_eq!(decisions.len(), 1);
    let mut answered = false;
    for _ in 0..200 {
        // Wait for the executor to reconnect before answering.
        if _orch
            .app
            .hub
            .is_online(_orch.app.store.executors().await.unwrap()[0].id)
        {
            c.call::<DecisionRespond>(DecisionRespondParams {
                decision_id: decisions[0].id,
                option_id: "answer".into(),
                expected_state: None,
                answer: Some("SQLite".into()),
            })
            .await
            .unwrap();
            answered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(answered, "executor reconnected");
    until(&c, id, "success", |t| t.state == TaskState::Succeeded).await;
    assert!(
        std::fs::read_to_string(rig.ws.join("answer.txt"))
            .unwrap()
            .contains("SQLite")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_executor_crash_mid_worker_is_unknown_until_you_retry() {
    let rig = rig();
    let orch = Orch::start(&rig).await;
    let (exec, handle) = start_executor(&rig.config).await;
    let id = orch.delegate("slowonce").await;
    let c = client(&rig).await;
    until(&c, id, "running", |t| t.state == TaskState::Running).await;
    for _ in 0..200 {
        if rig.ws.join(".slowonce").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    exec.abort_all();
    handle.abort();
    let (_exec2, _h2) = start_executor(&rig.config).await;
    let t = until(&c, id, "outcome unknown", |t| {
        t.state == TaskState::OutcomeUnknown
    })
    .await;
    assert_eq!(t.blocked, None);
    assert!(
        !rig.ws.join("retried.txt").exists(),
        "nothing re-ran on its own"
    );

    let retried = c
        .call::<TaskRetry>(TaskGetParams { task_id: id })
        .await
        .unwrap();
    assert!(matches!(
        retried.state,
        TaskState::Queued | TaskState::Running
    ));
    until(&c, id, "success after retry", |t| {
        t.state == TaskState::Succeeded
    })
    .await;
    assert!(rig.ws.join("retried.txt").exists());
    let attempts = orch.app.store.attempts_for(id).await.unwrap();
    assert_eq!(attempts.len(), 2, "a retry is a new attempt");
    assert_eq!(attempts[0].outcome.as_deref(), Some("interrupted"));
}

#[tokio::test(flavor = "multi_thread")]
async fn you_can_settle_an_unknown_outcome_by_hand() {
    let rig = rig();
    let orch = Orch::start(&rig).await;
    let (exec, handle) = start_executor(&rig.config).await;
    let id = orch.delegate("slow").await;
    let c = client(&rig).await;
    until(&c, id, "running", |t| t.state == TaskState::Running).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    exec.abort_all();
    handle.abort();
    let (_exec2, _h2) = start_executor(&rig.config).await;
    until(&c, id, "outcome unknown", |t| {
        t.state == TaskState::OutcomeUnknown
    })
    .await;
    let t = c
        .call::<TaskResolve>(TaskResolveParams {
            task_id: id,
            outcome: ResolvedOutcome::Failed,
        })
        .await
        .unwrap();
    assert_eq!(t.state, TaskState::Failed);
}
