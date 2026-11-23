# State and durability

Where state lives and how it survives crashes and disconnects. Summary in [DESIGN.md](../../DESIGN.md).

## Store

- **One SQLite file** (`~/.console/orchestrator.db`) in WAL mode with `synchronous=FULL`, accessed through `sqlx` with versioned migrations.
- **One writer task** serializes writes; reads run concurrently.
- **Portable SQL**, so a cloud Orchestrator can move to Postgres by changing queries, not callers.

| Table | Holds |
|---|---|
| `sessions` | The Orchestrator conversation and each worker session |
| `turns` | Turn records ([agent](orchestrator-agent.md#turn-records)) |
| `todos`, `todo_deps`, `proposals` | The queue ([queue](priority-queue.md)) |
| `tasks`, `attempts`, `commands` | Delegated work and its dispatches |
| `decisions`, `artifacts` | Pending questions and permissions; briefs and patches |
| `executors`, `workspaces` | Registered executors and their workspaces |
| `events` | The event log |

The executor keeps its own `journal.db` ([executors](executors.md#commands-and-the-journal)).

## Event log

- **Rows hold current state; `events` is an append-only history** (`seq, at, session_id?, task_id?, todo_id?, kind, payload`). It is not event sourcing: nothing rebuilds state by replaying it.
- **Outbox rule:** a state change and its event are written in the same transaction. Events are pushed to subscribers only after commit.
- **Cursor replay:** a client subscribes with the last `seq` it saw and receives what it missed, then live events. The server subscribes to the live feed before reading the backlog and drops duplicates by `seq`, so nothing slips through the gap.
- **Uses:** client reconnects, audit, and later the grounding for digests.
- Worker output is coalesced before it is stored, so the log doesn't hold one row per token.

## Conditional transitions

Every state change is compare-and-set inside a transaction:

```rust
let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;   // take the write lock up front
let next = task_machine::apply(current, &event)?;          // pure; rejects illegal moves
let n = sqlx::query("UPDATE tasks SET state = ?, blocked_reason = ?, version = version + 1
                     WHERE id = ? AND state = ? AND version = ?")
    /* binds */ .execute(&mut *tx).await?.rows_affected();
if n != 1 { return Err(Conflict); }                         // rollback: no change, no event
insert_event(&mut tx, /* TaskTransition */).await?;
tx.commit().await?;                                         // then broadcast
```

- **Illegal transitions** are rejected by the state machine before any SQL runs.
- **Lost or double transitions** are prevented by the `WHERE` clause. On conflict the caller re-reads.
- **Duplicate messages** (the link is at-least-once) are no-ops when the task is already in the target state.
- **`BEGIN IMMEDIATE`** avoids `SQLITE_BUSY` from upgrading a read lock mid-transaction.
- The same pattern, with `expected_version`, covers todos.

## Task state machine

```
Queued ──attempt accepted──► Running ──result──► Succeeded / Failed
  │                            │
  └─cancel─► Cancelled         ├─cancel─► CancelRequested ──confirmed──► Cancelled
                               │                 └─ work finished first ─► Succeeded / Failed
                               └─can't verify──► OutcomeUnknown
                                                    ├─ reconciled ─► Succeeded / Failed / Cancelled
                                                    └─ you authorize a retry ─► Queued (new attempt)
```

- **A task exists only after `delegate_task`.** Conversation turns are not tasks.
- **Cancelling is a request.** `CancelRequested` holds until the executor confirms; the work may finish first.
- **`OutcomeUnknown` is honest uncertainty.** After an executor crash, nobody knows whether the worker pushed. It is reconciled against the journal or decided by you, never retried silently.
- **A retry is a new attempt;** the task keeps its history.
- **Terminal states are final.**
- **Blocked reasons sit beside the state:** `WaitingExecutor` (executor offline), `AwaitingInput` (worker asked a question), `AwaitingDecision` (permission or decision pending). "Queued while the laptop executor is offline" is `Queued` with `WaitingExecutor`, not a separate state.
- **Property tests** run random event sequences against the pure machine and check the invariants: terminal states never change, `Running` is reached only through an accepted attempt, and blocked reasons clear on terminal states.

## What durability does not cover

The store can't know whether the outside world matches it. If a worker dies mid-`git push`, the task is correctly `OutcomeUnknown`; only reconciliation or you can settle it: `retry` starts a fresh attempt, and `resolve` records what actually happened (both also exist as Orchestrator tools, used only when you say so). Resuming an interrupted worker's session (rather than starting over) is a later improvement.

Covered end to end by `crates/orchestrator/tests/recovery.rs`: the Orchestrator crashing while a worker waits on you, the executor crashing mid-worker, retry, and manual resolution.
