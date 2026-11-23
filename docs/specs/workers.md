# Workers

Coding agents that do delegated work in a real workspace. Summary in [DESIGN.md](../../DESIGN.md).

## Harnesses

| Harness | Adapter | Status |
|---|---|---|
| Claude Code | `@agentclientprotocol/claude-agent-acp` (0.109.1 in M0) | Verified in M0 |
| Codex | `@agentclientprotocol/codex-acp` (2.9.0 on npm; Codex CLI 0.163.0 installed) | Spike at the start of M7 |

- **Both speak ACP,** so the executor's worker host (`acp-host`) is harness-agnostic. A harness is a config entry: name, command, and environment.
- **`delegate_task` takes an optional `harness`.** The default is Claude Code; you or the brief can ask for Codex.
- **Versions are pinned and installed,** not fetched with `npx` at runtime (cold start was about 5 s in M0).

**Codex spike checklist:** MCP servers passed through ACP `session/new` reach Codex; permission requests arrive as ACP requests; cancel stops the turn; how auth works (ChatGPT login or API key); how its sandbox modes map to our permission policy.

## Lifecycle

1. `delegate_task` stores the brief as an artifact and creates the task.
2. The Orchestrator dispatches `StartWorker {task_id, workspace_id, harness, brief}` to the executor.
3. The executor starts the adapter in its own process group, opens an ACP session in the workspace with our worker MCP server attached, and sends the brief as the first prompt.
4. ACP updates stream back as `Progress` messages and are persisted as events.
5. The task ends when the worker calls `complete`, fails, or is cancelled. The executor captures a patch (`git diff` against the base revision) and kills the process group.

## Briefs

A brief has a title, goal, relevant context from the conversation (including inline findings), constraints, acceptance criteria, suggested checks, the workspace, and the linked todo. It is the worker's whole view of the task, so it must stand alone.

## Worker tools

The executor hosts an MCP server for each worker and proxies calls to the Orchestrator (`McpCall`).

| Tool | Effect |
|---|---|
| `ask_orchestrator(question)` | Task becomes blocked with `AwaitingInput`. The question appears in your conversation, and your answer returns as the worker's next prompt. |
| `report_progress(note)` | Recorded as an event, visible on request |
| `complete(summary, evidence)` | Declares the task done |
| `propose_followup(todo)` | Adds a proposed todo to the queue's inbox |
| `mark_blocked(reason)` | Task becomes blocked with the reason |

A turn that ends without `complete` or a question is treated as needing input. M0 showed a worker backgrounding a command and ending its turn early, so silence is never read as success.

## Mediation

- Worker questions and ACP permission requests become decisions in the Orchestrator conversation. You answer there.
- `/attach <task>` opens the worker's own session in Toad to watch or steer it directly.

## Concurrency

At most one active worker per workspace and two in total. Extra delegations wait as `ready` todos, and the Orchestrator says so.

## Permissions

**Decided November 23, 2026: workers inherit your Claude Code and Codex settings.** The executor passes no permission overrides. Any permission request a worker still raises becomes a decision in the Orchestrator conversation.
