//! Wire-shape tests. Snapshots pin the exact JSON each message produces; round trips
//! check that parsing the JSON gives back the same value. A snapshot diff is a protocol
//! change and needs a deliberate review (and a version bump when incompatible).

use chrono::{DateTime, TimeZone, Utc};
use console_proto::api::*;
use console_proto::domain::*;
use console_proto::event::*;
use console_proto::link::*;
use console_proto::*;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::json;
use ulid::Ulid;

/// JSON snapshot with map keys sorted, so the result doesn't depend on whether another
/// workspace crate enables `serde_json/preserve_order` (feature unification).
macro_rules! snap {
    ($value:expr) => {
        insta::with_settings!({ sort_maps => true }, { insta::assert_json_snapshot!($value) })
    };
}

/// Deterministic IDs: a fixed timestamp plus a small distinguishing number.
fn ulid(n: u128) -> Ulid {
    Ulid::from_parts(1_763_800_000_000, n)
}

fn at(min: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 22, 19, min, 0).unwrap()
}

fn score(v: u8) -> Score {
    Score::try_from(v).unwrap()
}

fn round_trip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(value: &T) {
    let json = serde_json::to_string(value).unwrap();
    let back: T = serde_json::from_str(&json).unwrap();
    assert_eq!(&back, value, "round trip changed the value: {json}");
}

fn session() -> SessionId {
    SessionId::from_ulid(ulid(1))
}
fn task() -> TaskId {
    TaskId::from_ulid(ulid(2))
}
fn todo() -> TodoId {
    TodoId::from_ulid(ulid(3))
}
fn turn() -> TurnId {
    TurnId::from_ulid(ulid(4))
}
fn workspace() -> WorkspaceId {
    WorkspaceId::from_ulid(ulid(5))
}
fn command() -> CommandId {
    CommandId::from_ulid(ulid(6))
}
fn attempt() -> AttemptId {
    AttemptId::from_ulid(ulid(7))
}
fn executor() -> ExecutorId {
    ExecutorId::from_ulid(ulid(8))
}
fn decision() -> DecisionId {
    DecisionId::from_ulid(ulid(9))
}

fn event(seq: u64, body: EventBody) -> Event {
    Event {
        seq: EventSeq(seq),
        at: at(30),
        session_id: Some(session()),
        task_id: None,
        todo_id: None,
        body,
    }
}

fn todo_view() -> TodoView {
    TodoView {
        id: todo(),
        title: "Fix flaky auth test (repo-x)".into(),
        kind: TodoKind::Bugfix,
        owner: TodoOwner::Orchestrator,
        status: TodoStatus::Active,
        band: Band::from_scores(score(3), score(4)),
        importance: score(3),
        urgency: score(4),
        due_at: Some(at(59)),
        why: "P0 · due today · unblocks 2".into(),
        version: 3,
    }
}

fn decision_view() -> DecisionView {
    DecisionView {
        id: decision(),
        task_id: task(),
        question: "Allow `cargo test -p auth`?".into(),
        options: vec![
            DecisionOption {
                id: "allow_once".into(),
                label: "Allow once".into(),
            },
            DecisionOption {
                id: "reject".into(),
                label: "Reject".into(),
            },
        ],
    }
}

#[test]
fn ids() {
    snap!(json!({
        "session": session(), "turn": turn(), "todo": todo(), "task": task(),
        "attempt": attempt(), "command": command(), "executor": executor(),
        "workspace": workspace(), "decision": decision(),
        "artifact": ArtifactId::from_ulid(ulid(10)), "digest": DigestId::from_ulid(ulid(11)),
        "proposal": ProposalId::from_ulid(ulid(12)), "terminal": TerminalId::from_ulid(ulid(13)),
        "seq": EventSeq(42), "epoch": AuthorityEpoch(2),
    }));
}

#[test]
fn events() {
    let mut delegated = event(
        3,
        EventBody::TaskDelegated {
            title: "Fix flaky auth test".into(),
            workspace_id: workspace(),
        },
    );
    delegated.task_id = Some(task());
    delegated.todo_id = Some(todo());
    let mut transition = event(
        4,
        EventBody::TaskTransition {
            from: TaskState::Running,
            to: TaskState::Running,
            blocked: Some(BlockedReason::AwaitingInput),
        },
    );
    transition.task_id = Some(task());

    let events = vec![
        event(
            1,
            EventBody::UserMessage {
                turn_id: turn(),
                text: "fix the flaky auth test".into(),
            },
        ),
        event(
            2,
            EventBody::ToolCallStarted {
                turn_id: Some(turn()),
                tool_call_id: "tc-1".into(),
                title: "ws_read_file repo-x src/auth.rs".into(),
            },
        ),
        delegated,
        transition,
        event(
            5,
            EventBody::ToolCallFinished {
                tool_call_id: "tc-1".into(),
                ok: true,
                summary: None,
            },
        ),
        event(
            6,
            EventBody::AgentText {
                turn_id: Some(turn()),
                text: "Handing this to a worker.".into(),
            },
        ),
        event(
            7,
            EventBody::TurnEnded {
                turn_id: turn(),
                decision: DelegationDecision::Delegate,
                tool_calls: 2,
                wall_ms: 4_210,
                budget_hit: false,
            },
        ),
        event(
            8,
            EventBody::DecisionRequested {
                decision_id: decision(),
                question: "Allow `cargo test -p auth`?".into(),
                options: decision_view().options,
            },
        ),
        event(
            9,
            EventBody::DecisionResolved {
                decision_id: decision(),
                option_id: "allow_once".into(),
            },
        ),
        event(
            10,
            EventBody::DigestDelivered {
                digest_id: DigestId::from_ulid(ulid(11)),
                level: DisclosureLevel::L1,
                headline: "T-123: leeway added, tests pass; needs your call on config".into(),
                needs_you: true,
                markdown: "- src/auth.rs (+38 -4)\n- 51 tests passed".into(),
            },
        ),
        event(
            11,
            EventBody::TodoChanged {
                title: "Fix flaky auth test".into(),
                band: Band::P0,
                status: TodoStatus::Active,
                version: 4,
            },
        ),
        event(
            12,
            EventBody::ExecutorStatus {
                executor_id: executor(),
                online: false,
            },
        ),
    ];
    for e in &events {
        round_trip(e);
    }
    snap!(events);
}

#[test]
fn client_api_params_and_results() {
    let attach = AttachResult {
        session_id: session(),
        kind: SessionKind::Orchestrator,
        title: "Orchestrator".into(),
        recent_events: vec![event(
            1,
            EventBody::UserMessage {
                turn_id: turn(),
                text: "hi".into(),
            },
        )],
        open_decisions: vec![decision_view()],
        queue_top: vec![todo_view()],
        cursor: EventSeq(1),
    };
    round_trip(&attach);
    let task_view = TaskView {
        id: task(),
        todo_id: todo(),
        title: "Fix flaky auth test".into(),
        state: TaskState::Running,
        blocked: Some(BlockedReason::AwaitingDecision),
        workspace_id: workspace(),
        created_at: at(2),
        updated_at: at(4),
    };
    round_trip(&task_view);
    let todo_add = TodoAddParams {
        title: "Bump tokio".into(),
        body: None,
        kind: Some(TodoKind::Chore),
        importance: Some(score(1)),
        urgency: None,
        due_at: None,
        workspace_id: Some(workspace()),
    };
    round_trip(&todo_add);
    let todo_move = TodoMoveParams {
        todo_id: todo(),
        before: Some(TodoId::from_ulid(ulid(30))),
        after: None,
        expected_version: 3,
    };
    round_trip(&todo_move);

    snap!(json!({
        "session.attach": {
            "params": [
                AttachParams { target: SessionTarget::Orchestrator, recent: 200 },
                AttachParams { target: SessionTarget::Task { task_id: task() }, recent: 50 },
            ],
            "result": attach,
        },
        "events.subscribe": {
            "params": SubscribeParams { session_id: session(), after: EventSeq(41) },
            "result": SubscribeResult { subscription: "sub-1".into() },
        },
        "turn.submit": {
            "params": TurnSubmitParams { text: "what's on my queue?".into(), idempotency_key: "k-1".into() },
            "result": TurnSubmitResult { turn_id: turn(), duplicate: false },
        },
        "turn.override": TurnOverrideParams { turn_id: turn(), mode: TurnOverride::Delegate },
        "worker.prompt": WorkerPromptParams { task_id: task(), text: "use the env var".into(), idempotency_key: "k-2".into() },
        "task.get": { "params": TaskGetParams { task_id: task() }, "result": task_view },
        "task.list": TaskListParams::default(),
        "task.cancel": { "params": TaskCancelParams { task_id: task() }, "result": TaskCancelResult { state: TaskState::CancelRequested } },
        "decision.respond": DecisionRespondParams { decision_id: decision(), option_id: "allow_once".into(), expected_state: Some(TaskState::Running) },
        "queue.list": QueueListParams::default(),
        "todo.add": todo_add,
        "todo.move": todo_move,
        "empty": Empty {},
    }));
}

#[test]
fn method_names_are_unique() {
    let mut names = METHOD_NAMES.to_vec();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), METHOD_NAMES.len());
    snap!(METHOD_NAMES);
}

#[test]
fn executor_messages() {
    let journal = vec![JournalEntry {
        command_id: command(),
        attempt_id: attempt(),
        status: CommandStatus::Interrupted,
        updated_at: at(10),
    }];
    let msgs = vec![
        ExecutorMsg::Hello {
            executor_id: executor(),
            protocol: LINK_PROTOCOL_VERSION,
            max_epoch_seen: AuthorityEpoch(1),
            capabilities: vec![
                Capability::Shell,
                Capability::Pty,
                Capability::Git,
                Capability::Worker {
                    harness: "claude".into(),
                },
            ],
            workspaces: vec![WorkspaceInfo {
                workspace_id: workspace(),
                name: "repo-x".into(),
                path: "/Users/me/src/repo-x".into(),
                repo_remote: Some("git@github.com:me/repo-x.git".into()),
                head_rev: Some("3f2a91c".into()),
                dirty: true,
            }],
            journal: journal.clone(),
        },
        ExecutorMsg::Accepted {
            command_id: command(),
        },
        ExecutorMsg::Duplicate {
            command_id: command(),
            status: CommandStatus::Running,
        },
        ExecutorMsg::Progress {
            command_id: command(),
            update: WorkerUpdate::Text {
                text: "Reading src/auth.rs".into(),
            },
        },
        ExecutorMsg::Progress {
            command_id: command(),
            update: WorkerUpdate::ToolCall {
                tool_call_id: "toolu_1".into(),
                title: "Write tests/auth_skew.rs".into(),
                kind: Some("edit".into()),
            },
        },
        ExecutorMsg::Progress {
            command_id: command(),
            update: WorkerUpdate::ToolCallDone {
                tool_call_id: "toolu_1".into(),
                ok: true,
            },
        },
        ExecutorMsg::Progress {
            command_id: command(),
            update: WorkerUpdate::Plan {
                entries: vec!["Add leeway".into(), "Add tests".into()],
            },
        },
        ExecutorMsg::Progress {
            command_id: command(),
            update: WorkerUpdate::Usage {
                used: 68_396,
                size: 1_000_000,
            },
        },
        ExecutorMsg::Result {
            command_id: command(),
            outcome: CommandOutcome::Succeeded {
                summary: Some("51 tests pass".into()),
                artifacts: vec![ArtifactId::from_ulid(ulid(10))],
            },
        },
        ExecutorMsg::Result {
            command_id: command(),
            outcome: CommandOutcome::Failed {
                error: "exit 101".into(),
            },
        },
        ExecutorMsg::Result {
            command_id: command(),
            outcome: CommandOutcome::Interrupted,
        },
        ExecutorMsg::ReconcileReport { entries: journal },
        ExecutorMsg::ReadOpResult {
            request_id: "r-1".into(),
            result: ReadOpOutcome::Ok {
                output: "fn verify_token".into(),
                truncated: false,
            },
        },
        ExecutorMsg::ReadOpResult {
            request_id: "r-2".into(),
            result: ReadOpOutcome::Denied {
                reason: "`rm` is not on the read-only allowlist".into(),
            },
        },
        ExecutorMsg::DecisionRequest {
            command_id: command(),
            decision_id: decision(),
            question: "Allow `cargo test -p auth`?".into(),
            options: decision_view().options,
        },
        ExecutorMsg::McpCall {
            call_id: "c-1".into(),
            command_id: command(),
            tool: "ask_orchestrator".into(),
            args: json!({"question": "env var or code?"}),
        },
        ExecutorMsg::TerminalOpened {
            terminal_id: TerminalId::from_ulid(ulid(13)),
            workspace_id: workspace(),
        },
        ExecutorMsg::TerminalClosed {
            terminal_id: TerminalId::from_ulid(ulid(13)),
            exit_code: Some(0),
        },
    ];
    for m in &msgs {
        round_trip(m);
    }
    snap!(msgs);
}

#[test]
fn orchestrator_messages() {
    let msgs = vec![
        OrchestratorMsg::Welcome {
            epoch: AuthorityEpoch(1),
            protocol: LINK_PROTOCOL_VERSION,
        },
        OrchestratorMsg::Reject {
            reason: "stale epoch".into(),
        },
        OrchestratorMsg::Dispatch {
            command_id: command(),
            attempt_id: attempt(),
            epoch: AuthorityEpoch(1),
            op: Op::StartWorker {
                task_id: task(),
                workspace_id: workspace(),
                harness: "claude".into(),
                brief: "# Fix flaky verify_token\n...".into(),
            },
        },
        OrchestratorMsg::Dispatch {
            command_id: command(),
            attempt_id: attempt(),
            epoch: AuthorityEpoch(1),
            op: Op::WorkerPrompt {
                task_id: task(),
                text: "Use the env var.".into(),
            },
        },
        OrchestratorMsg::Dispatch {
            command_id: command(),
            attempt_id: attempt(),
            epoch: AuthorityEpoch(1),
            op: Op::Exec {
                workspace_id: workspace(),
                argv: vec!["cargo".into(), "test".into()],
                timeout_ms: 600_000,
            },
        },
        OrchestratorMsg::Dispatch {
            command_id: command(),
            attempt_id: attempt(),
            epoch: AuthorityEpoch(1),
            op: Op::CapturePatch {
                workspace_id: workspace(),
                base_rev: "3f2a91c".into(),
            },
        },
        OrchestratorMsg::Cancel {
            command_id: command(),
            epoch: AuthorityEpoch(1),
        },
        OrchestratorMsg::Reconcile {
            command_ids: vec![command()],
        },
        OrchestratorMsg::ReadOp {
            request_id: "r-1".into(),
            op: ReadOp::Search {
                workspace_id: workspace(),
                pattern: "verify_token".into(),
                path_glob: Some("src/**".into()),
                max_matches: 50,
            },
            deadline_ms: 5_000,
        },
        OrchestratorMsg::DecisionResponse {
            decision_id: decision(),
            option_id: "allow_once".into(),
        },
        OrchestratorMsg::McpResult {
            call_id: "c-1".into(),
            result: json!({"answer": "env var"}),
            is_error: false,
        },
    ];
    for m in &msgs {
        round_trip(m);
    }
    snap!(msgs);
}

#[test]
fn read_ops() {
    let ops = vec![
        ReadOp::ListDir {
            workspace_id: workspace(),
            path: "src".into(),
        },
        ReadOp::ReadFile {
            workspace_id: workspace(),
            path: "src/auth.rs".into(),
            max_bytes: 65_536,
        },
        ReadOp::Search {
            workspace_id: workspace(),
            pattern: "TODO".into(),
            path_glob: None,
            max_matches: 20,
        },
        ReadOp::GitStatus {
            workspace_id: workspace(),
        },
        ReadOp::GitLog {
            workspace_id: workspace(),
            max: 10,
        },
        ReadOp::GitDiff {
            workspace_id: workspace(),
            rev_range: Some("main..HEAD".into()),
        },
        ReadOp::GitShow {
            workspace_id: workspace(),
            rev: "3f2a91c".into(),
        },
        ReadOp::RunReadonly {
            workspace_id: workspace(),
            argv: vec!["gh".into(), "pr".into(), "view".into(), "42".into()],
        },
    ];
    for op in &ops {
        round_trip(op);
    }
    snap!(ops);
}

#[test]
fn unknown_fields_and_variants_are_rejected_or_tolerated_deliberately() {
    // Unknown event kinds must fail loudly rather than be silently dropped.
    let bad = r#"{"seq":1,"at":"2026-11-22T19:30:00Z","kind":"no_such_kind"}"#;
    assert!(serde_json::from_str::<Event>(bad).is_err());
    // Malformed IDs are rejected at the boundary.
    let bad_id = r#"{"task_id":"todo_01KAQ7QJD0000000000000000Z"}"#;
    assert!(serde_json::from_str::<TaskGetParams>(bad_id).is_err());
}
