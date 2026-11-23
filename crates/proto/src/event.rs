//! The durable event log's record type.
//!
//! The envelope carries the routing keys (session, task, todo) that subscriptions filter on.
//! The body carries only the payload, so a key is never stored twice.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::{
    Band, BlockedReason, DelegationDecision, DisclosureLevel, TaskState, TodoStatus,
};
use crate::ids::{
    DecisionId, DigestId, EventSeq, ExecutorId, SessionId, TaskId, TodoId, TurnId, WorkspaceId,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub seq: EventSeq,
    pub at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub todo_id: Option<TodoId>,
    #[serde(flatten)]
    pub body: EventBody,
}

/// One option offered by a decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionOption {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventBody {
    /// The user sent a message in a session.
    UserMessage { turn_id: TurnId, text: String },
    /// Agent output text (coalesced, not per token).
    AgentText {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<TurnId>,
        text: String,
    },
    ToolCallStarted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<TurnId>,
        tool_call_id: String,
        title: String,
    },
    ToolCallFinished {
        tool_call_id: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
    /// An Orchestrator turn finished; the fields feed the turn record.
    TurnEnded {
        turn_id: TurnId,
        decision: DelegationDecision,
        tool_calls: u32,
        wall_ms: u64,
        budget_hit: bool,
    },
    /// The Orchestrator handed work to a worker. The envelope carries the task and todo.
    TaskDelegated {
        title: String,
        workspace_id: WorkspaceId,
    },
    TaskTransition {
        from: TaskState,
        to: TaskState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        blocked: Option<BlockedReason>,
    },
    DecisionRequested {
        decision_id: DecisionId,
        question: String,
        options: Vec<DecisionOption>,
    },
    DecisionResolved {
        decision_id: DecisionId,
        option_id: String,
    },
    /// A digest was delivered into a session at some level.
    DigestDelivered {
        digest_id: DigestId,
        level: DisclosureLevel,
        headline: String,
        needs_you: bool,
        markdown: String,
    },
    /// A queue item changed. Clients refresh their queue view from this.
    TodoChanged {
        title: String,
        band: Band,
        status: TodoStatus,
        version: u64,
    },
    ExecutorStatus {
        executor_id: ExecutorId,
        online: bool,
    },
}
