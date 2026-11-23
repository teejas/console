# Workers

Coding agents that do delegated work in a real workspace. Summary in [DESIGN.md](../../DESIGN.md).

## Harnesses

| Harness | Adapter | Status |
|---|---|---|
| Claude Code | `@agentclientprotocol/claude-agent-acp` 0.109.1 | Verified in M0 and M7 |
| Codex | `@agentclientprotocol/codex-acp` 2.9.0 (Codex CLI 0.163.0, ChatGPT login) | Verified in M7 |

- **Both speak ACP,** so the executor's worker host (`acp-host`) is harness-agnostic. A harness is a config entry, `executor.harnesses.<name> = [command, args...]`.
- **`delegate_task` takes an optional `harness`.** The default is `executor.default_harness` (`claude`); you or the brief can ask for Codex.
- **Versions are pinned and installed globally** (`npm install -g ...@<version>`), not fetched with `npx` at runtime (cold start was about 5 s in M0).
- **Both adapters advertise MCP over HTTP** (`mcpCapabilities.http`), so the executor serves worker tools the same way for both.
- **Process groups:** the ACP crate starts each adapter in its own process group and kills the group on shutdown or when the connection drops.

## Lifecycle

1. `delegate_task` stores the brief as an artifact and creates the task.
2. The Orchestrator dispatches `StartWorker {task_id, workspace_id, harness, brief}` to the executor.
3. The executor starts the adapter in its own process group, opens an ACP session in the workspace with our worker MCP server attached, and sends the brief as the first prompt.
4. ACP updates stream back as `Progress` messages and are persisted as events.
5. The task ends when the worker calls `complete`, fails, or is cancelled. The executor captures a patch (`git diff` against the base revision) and kills the process group.

## Briefs

A brief has a title, goal, relevant context from the conversation (including inline findings), constraints, acceptance criteria, suggested checks, the workspace, and the linked todo. It is the worker's whole view of the task, so it must stand alone.

## Worker tools

The executor serves the worker tools over streamable HTTP on a local port, behind the local token; each worker's requests carry its task in an `x-console-task` header. `propose_followup` is proxied to the Orchestrator (`McpCall`); the rest are handled by the executor.

| Tool | Effect |
|---|---|
| `ask_orchestrator(question)` | Task becomes blocked with `AwaitingInput`. The question appears in your conversation, and your answer returns as the worker's next prompt. |
| `report_progress(note)` | Recorded as an event, visible on request |
| `complete(summary, evidence)` | Declares the task done |
| `propose_followup(todo)` | Adds a proposed todo to the queue's inbox |
| `mark_blocked(reason)` | Task becomes blocked with the reason |

A turn that ends without `complete` or a question becomes a question to you ("The worker ended its turn without completing the task… What should it do next?"), and your answer is its next prompt. M0 showed a worker backgrounding a command and ending its turn early, so silence is never read as success.

When the worker completes, the executor captures the patch against the revision at start (untracked files included, index untouched) and reports it with the summary. Steering messages (`task_message`, `worker.prompt`, `console acp --task`) are sent as their own commands and never move the task's state.

## Mediation

- Worker questions and ACP permission requests become decisions in the Orchestrator conversation. You answer there.
- `/attach <task>` opens the worker's own session in Toad to watch or steer it directly.

## Concurrency

At most one active worker per workspace and two in total. Extra delegations wait as `ready` todos, and the Orchestrator says so.

## Permissions

**Decided November 23, 2026: workers inherit your Claude Code and Codex settings.** The executor passes no permission overrides. Any permission request a worker still raises becomes a decision in the Orchestrator conversation.
