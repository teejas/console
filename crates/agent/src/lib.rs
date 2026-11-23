//! The Orchestrator agent: system prompt, settings, hooks, MCP config, and briefs (M5).
//!
//! See `docs/specs/orchestrator-agent.md`. Everything here is pure: the service writes
//! these files and hosts the tools.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The MCP server name the agent sees; tools appear as `mcp__console__<tool>`.
pub const MCP_SERVER: &str = "console";

/// The tool that hands work to a worker; it stays allowed when the inline budget is spent.
pub const DELEGATE_TOOL: &str = "delegate_task";

/// The Orchestrator's role, delegation policy, and output style.
pub const SYSTEM_PROMPT: &str = r#"You are the Orchestrator: the user's long-lived engineering assistant and conversational partner. You keep a shared priority queue of todos in sync with the user and hand substantial work to coding agents ("workers") that run in the user's real workspaces.

# How to answer
- Lead with a one-line headline. Add a few short bullets only if they help. Stop there: the user asks for more when they want it ("more", "details", "show me").
- Always say clearly when something needs the user (a decision, a question from a worker, a failed or uncertain task).
- Refer to workspaces by name and to todos and tasks by title.

# Inline or delegate
Handle these yourself, inline: questions, explanations, planning, short read-only lookups (reading a file, searching, git status/log/diff, viewing a PR), queue and status operations, and drafting text that doesn't touch a workspace.

Delegate with `delegate_task` when work is non-trivial: any change to a workspace, anything likely to take more than a minute or two or many steps, builds and test runs, multi-step investigations, and non-trivial PR reviews. You cannot modify files or run builds yourself.

When you delegate, say so in one line with the reason ("Handing this to a worker: it touches several files."), then end your turn. The worker runs in the background; the conversation continues. If unsure, pick one and say which. Ask only when the target workspace or the goal is genuinely unclear.

Your inline lookups have a budget per turn. If a tool call is denied because the budget is spent, stop looking things up: answer with what you have, or call `delegate_task` and put your findings in the brief's context.

# Briefs
A brief is the worker's entire view of the task, so it must stand alone: a clear goal, the relevant context from this conversation (including what you already found), constraints, concrete acceptance criteria, and checks to run. Workers report back through the task; you'll see progress and results in the state injected at the start of each turn.

# The queue
- Todos have importance and urgency (1-4). Bands: P0 urgent and important, P1 urgent, P2 important, P3 neither. Urgent ranks above important.
- When you add a todo, propose sensible kind, importance, and urgency. Fields the user set are theirs: your edits to them become proposals with your rationale, which the user accepts or rejects.
- When the user asks what to do next, start from the top of the queue and explain why.

# State
Each turn starts with a state snapshot (queue top, active tasks, items waiting on the user, workspaces, the time). Trust it over earlier conversation when they differ.
"#;

/// Claude Code settings for the Orchestrator: only our MCP tools are allowed, and the two
/// hooks call back into the service through `console hook`.
#[must_use]
pub fn settings(console_bin: &str) -> Value {
    let hook = |name: &str| format!("'{}' hook {name}", console_bin.replace('\'', r"'\''"));
    json!({
        "permissions": {
            "allow": [format!("mcp__{MCP_SERVER}__*")],
            "deny": ["Bash", "Write", "Edit", "Read", "Glob", "Grep", "NotebookEdit", "WebFetch", "WebSearch", "Task", "Agent"]
        },
        "hooks": {
            "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": hook("prompt-submit") }] }],
            "PreToolUse": [{ "matcher": "*", "hooks": [{ "type": "command", "command": hook("pre-tool") }] }]
        }
    })
}

/// The MCP config pointing Claude Code at the service's HTTP tool server.
#[must_use]
pub fn mcp_config(url: &str, token: &str) -> Value {
    json!({
        "mcpServers": {
            MCP_SERVER: {
                "type": "http",
                "url": url,
                "headers": { "Authorization": format!("Bearer {token}") }
            }
        }
    })
}

/// What `delegate_task` receives. Rendered into the worker's first prompt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Brief {
    /// Short task title, for example "Fix flaky auth test".
    pub title: String,
    /// What the worker must accomplish.
    pub goal: String,
    /// Workspace name (see the state snapshot).
    pub workspace: String,
    /// Relevant context from the conversation, including anything you already looked up.
    #[serde(default)]
    pub context: String,
    /// Constraints: what not to change, style, limits.
    #[serde(default)]
    pub constraints: Vec<String>,
    /// Concrete criteria the worker must meet before calling the task complete.
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    /// Checks to run, for example `cargo test -p auth`.
    #[serde(default)]
    pub suggested_checks: Vec<String>,
    /// An existing todo this work belongs to (its ID), if any.
    #[serde(default)]
    pub todo_id: Option<String>,
    /// Worker harness: `claude` (default) or `codex`.
    #[serde(default)]
    pub harness: Option<String>,
    /// Importance 1-4 for a new todo, if the user indicated it.
    #[serde(default)]
    pub importance: Option<u8>,
    /// Urgency 1-4 for a new todo, if the user indicated it.
    #[serde(default)]
    pub urgency: Option<u8>,
}

/// Renders a brief as the worker's first prompt.
#[must_use]
pub fn render_brief(b: &Brief, task_ref: &str) -> String {
    let mut out = format!(
        "# Task {task_ref}: {}\n\n## Goal\n{}\n",
        b.title,
        b.goal.trim()
    );
    let section = |out: &mut String, title: &str, items: &[String]| {
        if !items.is_empty() {
            out.push_str(&format!("\n## {title}\n"));
            for i in items {
                out.push_str(&format!("- {}\n", i.trim()));
            }
        }
    };
    if !b.context.trim().is_empty() {
        out.push_str(&format!("\n## Context\n{}\n", b.context.trim()));
    }
    section(&mut out, "Constraints", &b.constraints);
    section(&mut out, "Acceptance criteria", &b.acceptance_criteria);
    section(&mut out, "Suggested checks", &b.suggested_checks);
    out.push_str(WORKER_FOOTER);
    out
}

/// How every worker reports back; stated in every brief because ending a turn alone is
/// ambiguous (a worker can background a command and stop early).
pub const WORKER_FOOTER: &str = "
## Reporting
You are a worker started by the user's Orchestrator. Use the `console` MCP tools:
- `report_progress` for notable milestones.
- `ask_orchestrator` when you need a decision or information you can't find. Then end your turn and wait for the answer.
- `complete` with a summary and evidence (checks run, results) when every acceptance criterion is met. The task is not done until you call it.
- `propose_followup` for related work you notice but shouldn't do now.
Don't background long-running commands and end your turn; run them to completion.
";

/// The `console hook` exchange: what Claude Code sent on stdin.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct HookInput {
    #[serde(default)]
    pub hook_event_name: String,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

/// Hook output that injects context into the turn.
#[must_use]
pub fn prompt_submit_output(context: &str) -> Value {
    json!({ "hookSpecificOutput": { "hookEventName": "UserPromptSubmit", "additionalContext": context } })
}

/// Hook output that denies a tool call, with the reason shown to the model.
#[must_use]
pub fn pre_tool_deny(reason: &str) -> Value {
    json!({ "hookSpecificOutput": { "hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": reason } })
}

/// Whether a (possibly prefixed) tool name is `delegate_task`.
#[must_use]
pub fn is_delegate(tool_name: &str) -> bool {
    tool_name == DELEGATE_TOOL || tool_name.ends_with(&format!("__{DELEGATE_TOOL}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brief_renders_all_sections_and_the_reporting_contract() {
        let b = Brief {
            title: "Fix flaky auth test".into(),
            goal: "Make tests/auth.rs pass reliably.".into(),
            workspace: "repo-x".into(),
            context: "Fails under clock skew; see verify_token.".into(),
            constraints: vec!["Don't change the public API".into()],
            acceptance_criteria: vec!["100 consecutive passes".into()],
            suggested_checks: vec!["cargo test -p auth".into()],
            todo_id: None,
            harness: None,
            importance: None,
            urgency: None,
        };
        let r = render_brief(&b, "T-1");
        for s in [
            "# Task T-1",
            "## Goal",
            "## Context",
            "## Constraints",
            "## Acceptance criteria",
            "## Suggested checks",
            "`complete`",
        ] {
            assert!(r.contains(s), "missing {s}:\n{r}");
        }
    }

    #[test]
    fn settings_allow_only_our_tools_and_wire_both_hooks() {
        let s = settings("/opt/console's/bin/console");
        assert_eq!(s["permissions"]["allow"][0], "mcp__console__*");
        let cmd = s["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(cmd.ends_with("hook pre-tool"));
        assert!(cmd.contains(r"console'\''s"), "quoted: {cmd}");
    }

    #[test]
    fn delegate_tool_names() {
        assert!(is_delegate("mcp__console__delegate_task"));
        assert!(is_delegate("delegate_task"));
        assert!(!is_delegate("mcp__console__ws_read_file"));
    }
}
