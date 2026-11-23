# Design

The living architecture of console. Each bullet is a design choice and its purpose; the linked specs go deeper. Background research is in [docs/research](docs/research/). The build order is in [PLAN.md](PLAN.md).

## Scope

- **MVP:** a local Orchestrator (a Claude Code agent you talk to through Toad), a local executor that runs Claude Code and Codex workers, and a shared priority queue.
- **Next:** cloud executors.
- **Then:** inbound events (PR review requests, Slack, email) that become todos, starting with polling.
- **Later:** progressive-disclosure digests and learned detail preferences, terminals, a cloud Orchestrator, Slack and desktop clients, generated views, voice.

## System shape

Spec: [topology](docs/specs/topology.md)

- **A Rust Orchestrator service hosts the Orchestrator agent as a separate process.** The service owns state, the queue, and tools, so work survives the model process being restarted, swapped, or moved.
- **Clients are thin and attach to the service.** Toad and the CLI can disconnect at any time without affecting work, and the same clients keep working when the Orchestrator moves to the cloud.
- **All workspace access goes through executors.** The Orchestrator never touches files itself, so it behaves the same locally and in the cloud, and access control lives in one place.
- **Single user with a local token.** Keeps the MVP small; authentication is a seam, not a feature.

## Orchestrator agent

Spec: [orchestrator-agent](docs/specs/orchestrator-agent.md)

- **Claude Code CLI in `stream-json` mode, on `claude-opus-5-5` at effort `medium`.** Reuses a maintained agent loop (compaction, caching, resume) and a single login instead of building our own.
- **Conversation first; a task exists only after an explicit `delegate_task`.** Questions and quick lookups stay fast, and only substantial work starts a worker.
- **Inline tools are read-only, with a per-turn budget (10 calls, 90 s) enforced by hooks.** Anything that changes a workspace or runs long ends up with a worker, whatever the model decides.
- **Only one prompt is in flight at a time.** Claude Code merges queued prompts into one turn (found in M0), so we serialize them.
- **Our records are authoritative; Claude Code's session is a resumable cache.** A restart resumes the session, and our records can rebuild it if that fails.
- **Every turn is recorded with its inline-or-delegate decision.** Gives us the data to tune the delegation policy later.

## Workers

Spec: [workers](docs/specs/workers.md)

- **Claude Code and Codex, both through ACP adapters.** One protocol for every harness, so adding one is configuration rather than code.
- **The harness is chosen per delegation, defaulting to Claude Code.** You or the brief can pick Codex for a task.
- **A structured brief is the worker's first prompt.** The worker gets the goal, context, and acceptance criteria without needing the conversation.
- **Workers report through explicit tools (`ask_orchestrator`, `report_progress`, `complete`).** Ending a turn is ambiguous (M0), so completion and questions are signaled explicitly.
- **Worker questions and permission requests surface in the Orchestrator conversation.** You answer in one place, and can still attach to a worker to watch or steer it.
- **Each worker runs in its own process group, killed when the task ends.** No orphaned processes (seen in M0).
- **At most one worker per workspace and two in total.** Prevents conflicting edits and runaway cost.

## Executors

Spec: [executors](docs/specs/executors.md)

- **The executor is a separate daemon that holds a WebSocket link to the Orchestrator.** Same shape locally and in the cloud.
- **Commands are journaled before they run and deduplicated on redelivery.** Delivery is at-least-once without running anything twice.
- **Inline lookups are bounded, read-only ops; shell commands only from an allowlist.** The Orchestrator can look around but can't change anything without a worker.
- **Cloud executors (next) run the same binary on a cloud machine, with workspaces cloned from the remote.** Work can continue while the laptop sleeps, and results come back as a branch or patch.
- **Which side dials is configurable.** A local Orchestrator can't accept connections from the cloud, so it dials cloud executors instead.

## State and durability

Spec: [state-and-durability](docs/specs/state-and-durability.md)

- **SQLite in WAL mode, one file, through `sqlx`.** Transactions with nothing to operate; `sqlx` also speaks Postgres for a cloud Orchestrator.
- **Current-state tables plus an append-only event log, written in the same transaction.** Clients can replay what they missed without the cost of full event sourcing.
- **State transitions are compare-and-set on state and version.** No lost or illegal transitions, even with concurrent writers.
- **Tasks move through an explicit state machine that includes `OutcomeUnknown`.** Work with side effects is never silently re-run after a crash; it is reconciled or decided by you.
- **Clients resume from a cursor (the last event `seq` they saw).** Reconnecting never loses or duplicates updates.

## Shared priority queue

Spec: [priority-queue](docs/specs/priority-queue.md)

- **Bands P0–P3 from importance × urgency, with urgent ranked above important.** Time-sensitive requests clear first, and planned work keeps its order.
- **A score within each band, plus manual placement that sticks.** The order is computed by default and yours when you set it.
- **Every item has a `why`.** You can always see why something is where it is.
- **Fields you set change only through Orchestrator proposals.** The Orchestrator can suggest, but never silently overrides your priorities.
- **Edits carry an expected version.** Concurrent edits from you and the Orchestrator can't overwrite each other.

## Clients

Spec: [clients](docs/specs/clients.md)

- **Toad through `console acp`, an ACP bridge to the service.** Reuses a good TUI, and the same local TUI works against a cloud Orchestrator.
- **The queue appears as Toad's plan panel, with slash commands for common actions.** The queue stays visible during the conversation.
- **A `console` CLI for operations and scripting.** Status, queue, tasks, and decisions without a TUI.

## Inbound events

Spec: [inbound-events](docs/specs/inbound-events.md)

- **Every source (GitHub, Slack, email) is normalized into one event envelope.** Consumers don't care where an event came from.
- **Events are deduplicated on source ID and grouped by thread.** A PR's review request and its later comments update one todo instead of creating many.
- **A durable stream with a cursor per consumer.** New consumers can be added and replayed without touching producers.
- **Triage consumers turn events into todo proposals: rules first, then a cheap model call.** The Orchestrator's own conversation stays uncluttered.
- **Inbound content is untrusted, so triage has no tools with side effects.** An email can't make anything happen except a proposed todo.
- **Polling first; webhooks once there is a public endpoint.** Starts simple on a laptop, then moves to push when the Orchestrator moves to the cloud.

## Progressive disclosure

Spec: [progressive-disclosure](docs/specs/progressive-disclosure.md)

- **MVP: the Orchestrator answers headline-first and expands on request, by instruction.** Delivers the behavior without any pipeline.
- **Later: L0–L3 digests grounded in the event log, with detail preferences learned per kind and band.** Consistent updates at the level you want, tuned from how you react to them.

## Decided

- **Orchestrator isolation:** it shares your Claude Code login (November 23, 2026). No extra login; your skills and plugins are listed but inert because no tools are enabled.
- **Worker permissions:** workers inherit your Claude Code and Codex settings (November 23, 2026). Any permission request a worker does raise still becomes a decision in the conversation.

## Open decisions

- **Cloud executor host** (needed for cloud executors): where they run, and how credentials and repos are provisioned.
- **Event stream transport** (needed for inbound events): when to move from a SQLite table to a broker, and which one.
