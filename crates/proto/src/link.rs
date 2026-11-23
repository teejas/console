//! The executor link (`docs/specs/executors.md`).
//!
//! The executor dials the Orchestrator over WebSocket, in local mode as well as cloud
//! mode, and the two exchange these JSON messages. It is not JSON-RPC, because the
//! Orchestrator initiates most requests over a connection the executor opened.
//!
//! Delivery is at-least-once: the executor journals a command before acting on it, and
//! answers a repeated `Dispatch` with `Duplicate` rather than running it twice.
//! Terminal byte streams will use binary frames when terminals are added after the MVP.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event::DecisionOption;
use crate::ids::{
    AttemptId, AuthorityEpoch, CommandId, DecisionId, ExecutorId, TaskId, TerminalId, WorkspaceId,
};

/// Bumped on any incompatible change to the link protocol.
pub const LINK_PROTOCOL_VERSION: u32 = 1;

/// Messages sent by the executor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecutorMsg {
    /// The first message on every connection.
    Hello {
        executor_id: ExecutorId,
        /// Human-readable name from the executor's config.
        name: String,
        protocol: u32,
        /// Dispatches from an epoch lower than this are rejected.
        max_epoch_seen: AuthorityEpoch,
        capabilities: Vec<Capability>,
        workspaces: Vec<WorkspaceInfo>,
        /// Commands the executor knows about, so the Orchestrator can reconcile.
        journal: Vec<JournalEntry>,
    },
    /// The command was journaled and will run.
    Accepted {
        command_id: CommandId,
    },
    /// The command was already journaled; here is where it stands.
    Duplicate {
        command_id: CommandId,
        status: CommandStatus,
    },
    Progress {
        command_id: CommandId,
        update: WorkerUpdate,
    },
    Result {
        command_id: CommandId,
        outcome: CommandOutcome,
    },
    ReconcileReport {
        entries: Vec<JournalEntry>,
    },
    ReadOpResult {
        request_id: String,
        result: ReadOpOutcome,
    },
    /// A worker permission request or question that needs the user.
    DecisionRequest {
        command_id: CommandId,
        decision_id: DecisionId,
        question: String,
        options: Vec<DecisionOption>,
    },
    /// A worker called one of the Orchestrator's MCP tools; the executor proxies it.
    McpCall {
        call_id: String,
        command_id: CommandId,
        tool: String,
        args: Value,
    },
    TerminalOpened {
        terminal_id: TerminalId,
        workspace_id: WorkspaceId,
    },
    TerminalClosed {
        terminal_id: TerminalId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
    },
}

/// Messages sent by the Orchestrator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OrchestratorMsg {
    Welcome {
        epoch: AuthorityEpoch,
        protocol: u32,
    },
    /// The connection is refused (bad credential, stale epoch, protocol mismatch).
    Reject { reason: String },
    Dispatch {
        command_id: CommandId,
        attempt_id: AttemptId,
        epoch: AuthorityEpoch,
        op: Op,
    },
    Cancel {
        command_id: CommandId,
        epoch: AuthorityEpoch,
    },
    /// Report the status of these commands from the journal.
    Reconcile { command_ids: Vec<CommandId> },
    /// A bounded, read-only workspace lookup for the Orchestrator's inline tools.
    /// Not journaled: read ops are idempotent.
    ReadOp {
        request_id: String,
        op: ReadOp,
        deadline_ms: u64,
    },
    DecisionResponse {
        decision_id: DecisionId,
        option_id: String,
    },
    McpResult {
        call_id: String,
        result: Value,
        is_error: bool,
    },
}

/// Journaled operations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// Start a worker for a task; the brief becomes its first prompt.
    StartWorker {
        task_id: TaskId,
        workspace_id: WorkspaceId,
        harness: String,
        brief: String,
    },
    /// Send a follow-up prompt to a running worker (an answer, or steering).
    WorkerPrompt { task_id: TaskId, text: String },
    /// Run a non-agent command in a workspace.
    Exec {
        workspace_id: WorkspaceId,
        argv: Vec<String>,
        timeout_ms: u64,
    },
    /// Capture `git diff` against a base revision as a patch artifact.
    CapturePatch {
        workspace_id: WorkspaceId,
        base_rev: String,
    },
}

/// Read-only workspace lookups. Paths are relative to the workspace root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ReadOp {
    ListDir {
        workspace_id: WorkspaceId,
        path: String,
    },
    ReadFile {
        workspace_id: WorkspaceId,
        path: String,
        max_bytes: u64,
    },
    Search {
        workspace_id: WorkspaceId,
        pattern: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path_glob: Option<String>,
        max_matches: u32,
    },
    GitStatus {
        workspace_id: WorkspaceId,
    },
    GitLog {
        workspace_id: WorkspaceId,
        max: u32,
    },
    GitDiff {
        workspace_id: WorkspaceId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rev_range: Option<String>,
    },
    GitShow {
        workspace_id: WorkspaceId,
        rev: String,
    },
    /// Runs a command only if it is on the executor's read-only allowlist.
    RunReadonly {
        workspace_id: WorkspaceId,
        argv: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ReadOpOutcome {
    Ok {
        output: String,
        /// The output was cut at the size cap.
        truncated: bool,
    },
    Denied {
        reason: String,
    },
    Failed {
        error: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    Accepted,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    /// The executor restarted while the command was running; the effect is unknown.
    Interrupted,
    /// The executor has no record of the command.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub command_id: CommandId,
    pub attempt_id: AttemptId,
    pub status: CommandStatus,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum CommandOutcome {
    Succeeded {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        /// A unified diff of the workspace changes, stored by the Orchestrator as an artifact.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        patch: Option<String>,
    },
    Failed {
        error: String,
    },
    Cancelled,
    Interrupted,
}

/// Worker activity, translated from ACP session updates by the executor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "update", rename_all = "snake_case")]
pub enum WorkerUpdate {
    Text {
        text: String,
    },
    ToolCall {
        tool_call_id: String,
        title: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
    },
    ToolCallDone {
        tool_call_id: String,
        ok: bool,
    },
    Plan {
        entries: Vec<String>,
    },
    Usage {
        used: u64,
        size: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "capability", rename_all = "snake_case")]
pub enum Capability {
    Shell,
    Pty,
    Git,
    /// Can host workers for this harness (for example `claude`).
    Worker {
        harness: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub workspace_id: WorkspaceId,
    /// A short human name, unique per executor (for example `console`).
    pub name: String,
    /// Meaningful only on the owning executor.
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_remote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_rev: Option<String>,
    pub dirty: bool,
}
