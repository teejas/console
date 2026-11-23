//! The Orchestrator agent loop against a fake `claude` that speaks stream-json, plus
//! delegation, concurrency holds, the inline budget, and the hook endpoints.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use console_agent::Brief;
use console_client::Client;
use console_config::{Config, WorkspaceConfig};
use console_executor::Executor;
use console_orchestrator::{App, HOOK_PROMPT_SUBMIT, agent_runner, server, work};
use console_proto::api::{TurnSubmit, TurnSubmitParams};
use console_proto::domain::{TaskState, TodoStatus};
use console_proto::event::EventBody;
use console_proto::{EventSeq, TurnId};
use console_store::{EventFilter, Store};

const FAKE_CLAUDE: &str = r#"#!/usr/bin/env python3
import sys, json, os
args = sys.argv[1:]
with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "calls.log"), "a") as f:
    f.write(json.dumps(args) + "\n")
resume = args[args.index("--resume") + 1] if "--resume" in args else None
if resume and FAIL_RESUME:
    sys.stderr.write("No conversation found with session ID: " + resume + "\n")
    sys.exit(1)
sid = resume or "sess-fake-1"
def out(o):
    print(json.dumps(o), flush=True)
out({"type": "system", "subtype": "init", "session_id": sid})
for line in sys.stdin:
    text = json.loads(line)["message"]["content"]
    for chunk in ["Echo: ", text]:
        out({"type": "stream_event", "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": chunk}}})
    out({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "tu1", "name": "mcp__console__queue_list", "input": {}}]}})
    out({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "tu1", "content": "The queue is empty."}]}})
    out({"type": "result", "subtype": "success", "is_error": False, "session_id": sid, "total_cost_usd": 0.001, "result": "Echo: " + text})
"#;

struct Rig {
    dir: tempfile::TempDir,
    config: Config,
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

fn rig(fail_resume: bool) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("claude");
    std::fs::write(
        &fake,
        FAKE_CLAUDE.replace("FAIL_RESUME", if fail_resume { "True" } else { "False" }),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let ws = dir.path().join("repo-x");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("README.md"), "hi\n").unwrap();
    git(&ws, &["init", "-q", "-b", "main"]);
    git(&ws, &["add", "."]);
    git(&ws, &["commit", "-q", "-m", "init"]);
    let mut config = Config {
        data_dir: dir.path().join("data"),
        ..Config::default()
    };
    config.agent.claude_bin = fake;
    config.executor.workspaces = vec![WorkspaceConfig {
        name: "repo-x".into(),
        path: ws,
    }];
    config
        .executor
        .harnesses
        .insert("fake".into(), vec!["fake".into()]);
    config.executor.default_harness = "fake".into();
    Rig { dir, config }
}

async fn serve(rig: &mut Rig, store: Store) -> (Arc<App>, String) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    rig.config.orchestrator.listen = addr;
    rig.config.executor.orchestrator_url = format!("ws://{addr}/executor");
    let token = rig.config.token().unwrap();
    let app = App::new(rig.config.clone(), store, token.clone())
        .await
        .unwrap();
    tokio::spawn(server::serve(app.clone(), listener));
    agent_runner::start(&app);
    (app, token)
}

fn db(rig: &Rig) -> PathBuf {
    rig.dir.path().join("orchestrator.db")
}

async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..400 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for: {what}");
}

async fn turn_done(app: &App, id: TurnId) -> bool {
    app.store
        .turn(id)
        .await
        .unwrap()
        .is_some_and(|t| t.status == "done" || t.status == "failed")
}

async fn agent_text(app: &App) -> String {
    app.store
        .events_after(EventSeq::START, &EventFilter::All, 1000)
        .await
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::AgentText { text, .. } => Some(text),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_turn_streams_text_tool_calls_and_a_record() {
    let mut rig = rig(false);
    let (app, token) = {
        let store = Store::open(&db(&rig)).await.unwrap();
        serve(&mut rig, store).await
    };
    let (client, _) = Client::connect(&app.config.client_url(), &token)
        .await
        .unwrap();
    let r = client
        .call::<TurnSubmit>(TurnSubmitParams {
            text: "hello there".into(),
            idempotency_key: "k1".into(),
        })
        .await
        .unwrap();
    assert!(!r.duplicate);
    eventually("turn finished", || turn_done(&app, r.turn_id)).await;

    let turn = app.store.turn(r.turn_id).await.unwrap().unwrap();
    assert_eq!(turn.status, "done");
    assert_eq!(turn.cost_usd, Some(0.001));
    assert_eq!(agent_text(&app).await, "Echo: hello there");
    let events = app
        .store
        .events_after(EventSeq::START, &EventFilter::All, 1000)
        .await
        .unwrap();
    assert!(events.iter().any(|e| matches!(&e.body, EventBody::ToolCallStarted { title, .. } if title.starts_with("queue_list"))));
    assert!(
        events
            .iter()
            .any(|e| matches!(&e.body, EventBody::ToolCallFinished { ok: true, .. }))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e.body, EventBody::TurnEnded { .. }))
    );

    let again = client
        .call::<TurnSubmit>(TurnSubmitParams {
            text: "hello there".into(),
            idempotency_key: "k1".into(),
        })
        .await
        .unwrap();
    assert!(again.duplicate);
    assert_eq!(again.turn_id, r.turn_id);
}

#[tokio::test]
async fn a_restarted_service_resumes_the_agent_session() {
    let mut rig = rig(false);
    {
        let (app, _) = {
            let store = Store::open(&db(&rig)).await.unwrap();
            serve(&mut rig, store).await
        };
        let t = app.submit_turn("first", "a").await.unwrap();
        eventually("first turn", || turn_done(&app, t.turn_id)).await;
    }
    let (app, _) = {
        let store = Store::open(&db(&rig)).await.unwrap();
        serve(&mut rig, store).await
    };
    let t = app.submit_turn("second", "b").await.unwrap();
    eventually("second turn", || turn_done(&app, t.turn_id)).await;
    let calls = std::fs::read_to_string(rig.dir.path().join("calls.log")).unwrap();
    let last = calls.lines().last().unwrap();
    assert!(last.contains(r#""--resume", "sess-fake-1""#), "{last}");
}

#[tokio::test]
async fn a_failed_resume_starts_fresh_with_a_recap() {
    let mut rig = rig(true);
    let store = Store::open(&db(&rig)).await.unwrap();
    let (app, _) = serve(&mut rig, store).await;
    let sid = app.orchestrator_session;
    app.store
        .write(async |tx| tx.set_harness_session_ref(sid, "gone").await)
        .await
        .unwrap();
    let t = app.submit_turn("are you there?", "c").await.unwrap();
    eventually("turn", || turn_done(&app, t.turn_id)).await;
    assert_eq!(
        app.store.turn(t.turn_id).await.unwrap().unwrap().status,
        "done"
    );
    let text = agent_text(&app).await;
    assert!(text.contains("couldn't be resumed"), "{text}");
    assert!(text.contains("are you there?"));
}

fn brief(title: &str) -> Brief {
    Brief {
        title: title.into(),
        goal: "Do the thing".into(),
        workspace: "repo-x".into(),
        context: String::new(),
        constraints: vec![],
        acceptance_criteria: vec!["It works".into()],
        suggested_checks: vec![],
        todo_id: None,
        harness: None,
        importance: Some(3),
        urgency: None,
    }
}

async fn with_executor(rig: &mut Rig) -> Arc<App> {
    let (app, _) = serve(rig, Store::open_in_memory().await.unwrap()).await;
    let exec = Executor::open(rig.config.clone()).await.unwrap();
    tokio::spawn(exec.run_until(std::future::pending()));
    eventually("workspace", || async {
        app.store.find_workspace("repo-x").await.unwrap().is_some()
    })
    .await;
    app
}

#[tokio::test]
async fn delegation_creates_todo_brief_task_and_runs_a_worker() {
    let mut rig = rig(false);
    let app = with_executor(&mut rig).await;
    let msg = work::delegate(&app, brief("Fix flaky auth test"), None)
        .await
        .unwrap();
    assert!(msg.starts_with("Delegated as task"), "{msg}");
    let task = app.store.tasks(true).await.unwrap().pop().unwrap();
    eventually("worker finished", || async {
        app.store.task(task.id).await.unwrap().unwrap().state == TaskState::Succeeded
    })
    .await;
    let task = app.store.task(task.id).await.unwrap().unwrap();
    assert!(task.summary.unwrap().contains("Fix flaky auth test"));
    let brief = app
        .store
        .artifact(task.brief_artifact_id.unwrap())
        .await
        .unwrap()
        .unwrap();
    assert!(brief.content.contains("## Acceptance criteria"));
    assert!(
        app.store
            .worker_session_for(task.id)
            .await
            .unwrap()
            .is_some()
    );
    let todo = app.store.todo(task.todo_id).await.unwrap().unwrap();
    assert_eq!(todo.status, TodoStatus::Done);
    assert_eq!(todo.importance.get(), 3);

    let unknown = work::delegate(
        &app,
        Brief {
            workspace: "nope".into(),
            ..self::brief("x")
        },
        None,
    )
    .await
    .unwrap_err();
    assert!(unknown.contains("known workspaces: repo-x"), "{unknown}");
    let bad_harness = work::delegate(
        &app,
        Brief {
            harness: Some("gpt-9".into()),
            ..self::brief("y")
        },
        None,
    )
    .await
    .unwrap_err();
    assert!(bad_harness.contains("can't run"), "{bad_harness}");
}

#[tokio::test]
async fn concurrency_limits_hold_tasks_and_start_them_later() {
    let mut rig = rig(false);
    rig.config.orchestrator.max_workers_per_workspace = 1;
    let app = with_executor(&mut rig).await;
    let first = work::delegate(&app, brief("first"), None).await.unwrap();
    let second = work::delegate(&app, brief("second"), None).await.unwrap();
    assert!(first.starts_with("Delegated"), "{first}");
    assert!(second.contains("held it"), "{second}");
    for _ in 0..200 {
        let tasks = app.store.tasks(true).await.unwrap();
        if tasks.len() == 2 && tasks.iter().all(|t| t.state == TaskState::Succeeded) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let tasks = app.store.tasks(true).await.unwrap();
    let held = app.store.undispatched_tasks().await.unwrap();
    panic!(
        "tasks: {:?}\nheld: {:?}",
        tasks
            .iter()
            .map(|t| (&t.title, t.state, t.blocked))
            .collect::<Vec<_>>(),
        held.iter().map(|t| &t.title).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn the_inline_budget_denies_lookups_but_not_delegation() {
    let mut rig = rig(false);
    rig.config.agent.inline_max_tool_calls = 3;
    let (app, token) = serve(&mut rig, Store::open_in_memory().await.unwrap()).await;
    let sid = app.orchestrator_session;
    let (turn, _) = app
        .store
        .write(async |tx| tx.insert_turn(sid, "k", "look around").await)
        .await
        .unwrap();
    app.agent.begin_for_test(turn.id);
    for _ in 0..3 {
        assert_eq!(
            work::pre_tool(&app, "mcp__console__ws_read_file").await,
            None
        );
    }
    let denied = work::pre_tool(&app, "mcp__console__ws_search")
        .await
        .unwrap();
    assert!(denied.contains("delegate_task"), "{denied}");
    assert_eq!(
        work::pre_tool(&app, "mcp__console__delegate_task").await,
        None
    );
    assert!(app.store.turn(turn.id).await.unwrap().unwrap().budget_hit);

    // The hook endpoint returns Claude Code's hook JSON.
    let (client, _) = Client::connect(&app.config.client_url(), &token)
        .await
        .unwrap();
    let out = client
        .call_raw(
            HOOK_PROMPT_SUBMIT,
            serde_json::json!({"hook_event_name": "UserPromptSubmit"}),
        )
        .await
        .unwrap();
    let ctx = out["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(ctx.starts_with("STATE SNAPSHOT"), "{ctx}");
    assert!(ctx.contains("Queue"));
    let deny = client
        .call_raw(
            console_orchestrator::HOOK_PRE_TOOL,
            serde_json::json!({"tool_name": "mcp__console__git_log"}),
        )
        .await
        .unwrap();
    assert_eq!(deny["hookSpecificOutput"]["permissionDecision"], "deny");
}
