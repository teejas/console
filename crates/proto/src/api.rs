//! The Orchestrator's client API (`docs/specs/clients.md`): JSON-RPC 2.0 over WebSocket.
//!
//! Each method is a type implementing [`Method`], which ties its wire name to its params
//! and result types. Server-pushed events use the [`EVENT_NOTIFICATION`] notification.
//! This first cut covers the conversation, tasks, decisions, and the queue; later
//! iterations add digests, preferences, and terminals.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::domain::{
    Band, BlockedReason, Score, SessionKind, TaskState, TodoKind, TodoOwner, TodoStatus,
    TurnOverride,
};
use crate::event::{DecisionOption, Event};
use crate::ids::{
    DecisionId, EventSeq, ExecutorId, ProposalId, SessionId, TaskId, TodoId, TurnId, WorkspaceId,
};

/// Bumped on any incompatible change to the client API.
pub const API_VERSION: u32 = 1;

/// Notification method the server uses to push events to subscribers.
pub const EVENT_NOTIFICATION: &str = "events.event";

/// A client API method: its wire name and its params and result types.
pub trait Method {
    const NAME: &'static str;
    type Params: Serialize + DeserializeOwned;
    type Result: Serialize + DeserializeOwned;
}

macro_rules! method {
    ($(#[$meta:meta])* $name:ident, $wire:literal, $params:ty, $result:ty) => {
        $(#[$meta])*
        #[derive(Debug)]
        pub struct $name;
        impl Method for $name {
            const NAME: &'static str = $wire;
            type Params = $params;
            type Result = $result;
        }
    };
}

/// An empty params or result object (`{}`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Empty {}

// ---- Sessions and events ----

/// Which conversation to attach to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionTarget {
    /// The single long-lived Orchestrator conversation.
    Orchestrator,
    /// A worker's session for a task.
    Task { task_id: TaskId },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttachParams {
    pub target: SessionTarget,
    /// How many recent events to include in the snapshot.
    #[serde(default = "default_recent")]
    pub recent: u32,
}

const fn default_recent() -> u32 {
    200
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttachResult {
    pub session_id: SessionId,
    pub kind: SessionKind,
    pub title: String,
    /// The most recent events, oldest first.
    pub recent_events: Vec<Event>,
    pub open_decisions: Vec<DecisionView>,
    pub queue_top: Vec<TodoView>,
    /// Subscribe after this cursor to receive everything newer than the snapshot.
    pub cursor: EventSeq,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubscribeParams {
    pub session_id: SessionId,
    /// Events with a sequence number greater than this are replayed, then streamed live.
    pub after: EventSeq,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubscribeResult {
    pub subscription: String,
}

// ---- Turns and workers ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnSubmitParams {
    pub text: String,
    /// A resubmitted key returns the original turn instead of starting a new one.
    pub idempotency_key: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnSubmitResult {
    pub turn_id: TurnId,
    /// True when the key matched an existing turn.
    pub duplicate: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnOverrideParams {
    pub turn_id: TurnId,
    pub mode: TurnOverride,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkerPromptParams {
    pub task_id: TaskId,
    pub text: String,
    pub idempotency_key: String,
}

// ---- Tasks and decisions ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskView {
    pub id: TaskId,
    pub todo_id: TodoId,
    pub title: String,
    pub state: TaskState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked: Option<BlockedReason>,
    pub workspace_id: WorkspaceId,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskGetParams {
    pub task_id: TaskId,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskListParams {
    /// Include terminal tasks.
    #[serde(default)]
    pub include_done: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskCancelParams {
    pub task_id: TaskId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskCancelResult {
    /// The state after recording the request; confirmation arrives later as an event.
    pub state: TaskState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionView {
    pub id: DecisionId,
    pub task_id: TaskId,
    pub question: String,
    pub options: Vec<DecisionOption>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionRespondParams {
    pub decision_id: DecisionId,
    pub option_id: String,
    /// Rejected if the task is no longer in this state (the decision went stale).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_state: Option<TaskState>,
}

// ---- Queue ----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoView {
    pub id: TodoId,
    pub title: String,
    pub kind: TodoKind,
    pub owner: TodoOwner,
    pub status: TodoStatus,
    pub band: Band,
    pub importance: Score,
    pub urgency: Score,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_at: Option<DateTime<Utc>>,
    /// Why the item sits where it does, for `/why`.
    pub why: String,
    /// For optimistic concurrency on updates.
    pub version: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QueueListParams {
    #[serde(default)]
    pub include_done: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoAddParams {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Fields left out are proposed by triage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<TodoKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub importance: Option<Score>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub urgency: Option<Score>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoMoveParams {
    pub todo_id: TodoId,
    /// Place directly before this item, within the same band.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<TodoId>,
    /// Place directly after this item, within the same band.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<TodoId>,
    pub expected_version: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoUpdateParams {
    pub todo_id: TodoId,
    pub expected_version: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<TodoKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<TodoOwner>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<TodoStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub importance: Option<Score>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub urgency: Option<Score>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_at: Option<DateTime<Utc>>,
    /// Remove the due date.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_due: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<WorkspaceId>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProposalView {
    pub id: ProposalId,
    pub todo_id: TodoId,
    pub todo_title: String,
    pub field: String,
    pub value: serde_json::Value,
    pub rationale: String,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoUpdateResult {
    pub todo: TodoView,
    /// Changes to fields you own that became proposals instead (Orchestrator edits only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proposals: Vec<ProposalView>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProposalResolveParams {
    pub proposal_id: ProposalId,
    pub accept: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceView {
    pub id: WorkspaceId,
    pub name: String,
    pub executor_id: ExecutorId,
    pub executor_name: String,
    pub online: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_rev: Option<String>,
    pub dirty: bool,
}

method!(SessionAttach, "session.attach", AttachParams, AttachResult);
method!(
    EventsSubscribe,
    "events.subscribe",
    SubscribeParams,
    SubscribeResult
);
method!(
    /// Starts an Orchestrator turn. Never creates a task by itself.
    TurnSubmit,
    "turn.submit",
    TurnSubmitParams,
    TurnSubmitResult
);
method!(
    TurnOverrideMethod,
    "turn.override",
    TurnOverrideParams,
    Empty
);
method!(WorkerPrompt, "worker.prompt", WorkerPromptParams, Empty);
method!(TaskGet, "task.get", TaskGetParams, TaskView);
method!(TaskList, "task.list", TaskListParams, Vec<TaskView>);
method!(
    TaskCancel,
    "task.cancel",
    TaskCancelParams,
    TaskCancelResult
);
method!(
    DecisionRespond,
    "decision.respond",
    DecisionRespondParams,
    Empty
);
method!(QueueList, "queue.list", QueueListParams, Vec<TodoView>);
method!(TodoAdd, "todo.add", TodoAddParams, TodoView);
method!(TodoMove, "todo.move", TodoMoveParams, TodoView);
method!(
    TodoUpdate,
    "todo.update",
    TodoUpdateParams,
    TodoUpdateResult
);
method!(ProposalList, "proposal.list", Empty, Vec<ProposalView>);
method!(
    ProposalResolve,
    "proposal.resolve",
    ProposalResolveParams,
    TodoView
);
method!(WorkspaceList, "workspace.list", Empty, Vec<WorkspaceView>);

/// Every method name, for routing tables and docs.
pub const METHOD_NAMES: &[&str] = &[
    SessionAttach::NAME,
    EventsSubscribe::NAME,
    TurnSubmit::NAME,
    TurnOverrideMethod::NAME,
    WorkerPrompt::NAME,
    TaskGet::NAME,
    TaskList::NAME,
    TaskCancel::NAME,
    DecisionRespond::NAME,
    QueueList::NAME,
    TodoAdd::NAME,
    TodoMove::NAME,
    TodoUpdate::NAME,
    ProposalList::NAME,
    ProposalResolve::NAME,
    WorkspaceList::NAME,
];
