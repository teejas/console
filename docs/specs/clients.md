# Clients

How you interact with the Orchestrator. Summary in [DESIGN.md](../../DESIGN.md).

## Client API

JSON-RPC over WebSocket, served by the Orchestrator service. Types are in `crates/proto/src/api.rs`.

| Method | Behavior |
|---|---|
| `session.attach` | Snapshot of a session (the Orchestrator conversation or a worker session): recent events, open decisions, queue top, and a cursor |
| `events.subscribe` | Events after a cursor, then live events (pushed as `events.event` notifications) |
| `turn.submit {text, idempotency_key}` | Starts an Orchestrator turn. Does not create a task. |
| `turn.override` | Records `/inline` or `/delegate` |
| `worker.prompt` | Sends a message to a worker when attached to its session |
| `task.get`, `task.list`, `task.cancel` | Delegated work |
| `decision.respond` | Answers a question or permission request |
| `queue.list`, `todo.add`, `todo.move` (and more as the queue grows) | The shared queue |

Every client gets the same view, so you can start in one and continue in another.

## Toad

Toad talks ACP v1 to `console acp`, which bridges to the client API.

- **Default session:** the Orchestrator conversation. Text streams as message chunks; inline tool calls appear as collapsed tool calls; a delegation appears as a tool call titled like "Delegated → T-123: Fix flaky auth test (repo-x)".
- **Queue:** the top N items appear as ACP plan entries, so Toad's plan panel shows the queue.
- **Worker sessions:** `console acp --task <id>` replays and streams a worker's session and sends your prompts to it. `/attach <task>` launches it in a separate Toad instance.
- **Slash commands:** `/queue`, `/next`, `/why <n>`, `/add <title>`, `/done <n>`, `/proposals`, `/accept <n>`, `/reject <n>`, `/tasks`, `/decisions`, `/answer [n] <option or text>`, `/diff <task>`, `/cancel <task>`, `/attach <task>`, `/delegate`, `/inline`. They run through the client API without a model turn.
- **Notices:** task finished, failed, cancelled, outcome unknown, waiting for its executor; decisions and proposals that need you; an executor going offline. Each starts its own paragraph.
- **Plan updates** are sent only when the queue's top changes, because Toad also draws each plan inline in the transcript.

M0 findings that shape the bridge: out-of-turn updates render, the plan panel works, and replay during `session/new` renders. Consecutive chunks concatenate, so the bridge adds separators. Slash commands need Enter twice (the first picks the completion).

## CLI

`console` runs every role and the user commands, over the same client API as Toad:

| Command | Does |
|---|---|
| `orchestrator`, `executor`, `acp`, `hook` | The services, the Toad bridge, and Claude Code's hook entry points |
| `up`, `down`, `status` | Install, remove, and check the launchd agents; `status` also shows workspaces, tasks, and what's waiting on you |
| `ask` | Send one message and stream the reply |
| `queue`, `todo` | The shared queue |
| `tasks`, `logs [-f]`, `inspect` | Delegated work: list, a worker's session, and a readable account of a task, turn, or todo |
| `decide` | List and answer questions and permission requests |
| `retry`, `resolve` | Settle a task whose outcome is unknown |
| `config show`, `config path` | Inspect configuration |

## Later

Slack, a desktop app with generated views, and voice are further clients of the same API. None needs a protocol change.
