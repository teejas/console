# Orchestrator agent

The conversational agent you talk to. It answers inline when it can and delegates substantial work to workers. Summary in [DESIGN.md](../../DESIGN.md).

## Hosting

The service runs one long-lived Claude Code CLI process (`cc-host`) with these flags, all verified in M0 ([findings](../../spikes/README.md)):

| Flag | Purpose |
|---|---|
| `-p --input-format stream-json --output-format stream-json --verbose` | Programmatic turns with streamed events |
| `--model claude-opus-5-5 --effort medium` | The chosen model and effort |
| `--tools ""` | No built-in tools: no file access, shell, or subagents |
| `--strict-mcp-config --mcp-config <ours>` | Our MCP server is the only tool source |
| `--setting-sources "" --settings <ours>` | Ignore your personal settings; apply our permissions and hooks |
| `--system-prompt <ours>` | The Orchestrator role and delegation policy |
| `--resume <session_id>` | Continue after a restart |

Model access goes through Claude Code only; there is no direct API client.

## Turn loop

Each message you send is a **turn**, not a task.

1. The service persists the message and writes it to Claude Code. **One prompt in flight:** the next is sent only after the `result` message, because Claude Code merges queued prompts into one turn.
2. A `UserPromptSubmit` hook (`console hook prompt-submit`) injects current state as additional context: top of the queue, active tasks, items waiting on you, tasks finished in the last day, workspaces, and the time. The hooks reach the service over the client API using `CONSOLE_HOOK_URL` and `CONSOLE_HOOK_TOKEN`, which the service sets for the agent process (they are excluded from `CONSOLE_*` config overrides). A hook that can't reach the service fails open.
3. Streamed text and tool calls are translated to client events, persisted, and pushed to clients.
4. When the turn ends, the service writes a turn record.

**Durability:** our event log and turn records are authoritative. After a restart the service relaunches with `--resume`. If that fails, it starts a fresh session seeded with a summary built from our records.

## Tools

All tools come from the service's MCP server and appear to the agent as `mcp__console__*`.

| Group | Tools | Constraint |
|---|---|---|
| Workspace (through the executor) | `ws_list`, `ws_read_file`, `ws_search`, `git_status`, `git_log`, `git_diff`, `git_show`, `run_readonly` | Read-only by construction. `run_readonly` runs only allowlisted commands (for example `gh pr view`), with a timeout and an output cap. |
| Queue | `queue_list`, `todo_add`, `todo_update`, `todo_move`, `todo_drop`, `proposal_resolve` | Fields you set change only through proposals ([queue](priority-queue.md)) |
| Work | `delegate_task`, `task_status`, `task_message`, `task_cancel`, `decision_answer` | `delegate_task` is the only way work gets a worker |
| Context | `workspaces` | Registered workspaces and whether their executor is online |

The tools are served by the Orchestrator itself over streamable HTTP at `/mcp`, behind the local token.

## Delegation policy

**Soft policy, in the system prompt:**
- **Inline:** questions, explanations, planning, short read-only lookups, queue and status operations, and drafting text.
- **Delegate:** any workspace change; anything likely to take more than a minute or two; builds and test runs; multi-step investigations; non-trivial PR reviews.
- **When unsure,** pick one and say so in a line ("Handing this to a worker: it touches several files"). Ask only when the workspace or goal is unclear.

**Hard guardrails, enforced outside the model:**
1. **Inline work is read-only.** No built-in tools, and our workspace tools can't write.
2. **Per-turn budget:** 10 tool calls and 90 s (configurable). A `PreToolUse` hook (`console hook pre-tool`) counts calls. Near the limit it lets the call through with a note to wrap up; at the limit it denies everything except `delegate_task`, with a reason telling the agent to delegate. The turn's findings go into the brief.
3. **Delegation never blocks the conversation.** The turn ends with the handoff message.
4. **Concurrency** is enforced when `delegate_task` runs ([workers](workers.md#concurrency)).

**Overrides:** `/delegate` forces a handoff of the last request; `/inline` forces an inline answer within the guardrails. Both are recorded.

## Turn records

Each turn stores the decision (`inline`, `delegate`, or `ask`) and the stated reason, the inline tool count and wall time, whether the budget was hit, any override, and for delegated work the task's eventual outcome. In the MVP these are only recorded. Later they feed a labeled set and a `console eval delegation` replay for tuning the policy.

## Isolation

**Decided November 23, 2026: share your Claude Code login.** Claude Code still lists your skills and plugins even with `--setting-sources ""`, but with no tools enabled they can't do anything. A separate `CLAUDE_CONFIG_DIR` would isolate fully at the cost of a one-time `/login`; it remains an option if the shared setup ever leaks behavior.
