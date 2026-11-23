# Executors

Daemons that own workspaces and run workers. Summary in [DESIGN.md](../../DESIGN.md).

## Link

The executor and the Orchestrator exchange JSON messages over one WebSocket. The types are in `crates/proto/src/link.rs`.

| Direction | Messages |
|---|---|
| Executor → Orchestrator | `Hello` (identity, protocol, capabilities, workspaces, journal), `Accepted`, `Duplicate`, `Progress`, `Result`, `ReconcileReport`, `ReadOpResult`, `DecisionRequest`, `McpCall` |
| Orchestrator → Executor | `Welcome`, `Reject`, `Dispatch`, `Cancel`, `Reconcile`, `ReadOp`, `DecisionResponse`, `McpResult` |

- **Handshake:** `Hello` then `Welcome`. The executor reports its journal, so the Orchestrator can reconcile anything in flight.
- **Pairing:** a local token in the MVP; scoped and revocable device tokens for cloud executors.
- **Epoch:** every dispatch carries an authority epoch, and the executor rejects older ones. It stays at 1 until the Orchestrator can move between hosts.

## Commands and the journal

- **Journal before acting.** On `Dispatch`, the executor writes the command to `journal.db`, replies `Accepted`, then runs it.
- **Deduplicate.** A repeated `Dispatch` for a known command gets `Duplicate` with its current status, never a second run.
- **Reconcile on reconnect.** The Orchestrator asks about commands it thinks are in flight. A command that was running when the executor restarted reports `Interrupted`, and the task becomes `OutcomeUnknown` ([state](state-and-durability.md#task-state-machine)).
- **Operations:** `StartWorker`, `WorkerPrompt`, `Exec` (a non-agent command), and `CapturePatch`.

## Read ops

Inline lookups for the Orchestrator: list a directory, read a file, search, git status, log, diff, and show, and `RunReadonly`.

- **Not journaled,** because they are idempotent.
- **Bounded:** a deadline, a size cap (marked `truncated`), and a match limit.
- **`RunReadonly`** runs only commands on the executor's allowlist (`gh pr view`, `gh pr diff`, `gh pr checks`, `cargo metadata` by default). Anything else returns `Denied`.

## Workspaces

Each registered workspace has an ID, a path (meaningful only on its executor), a remote, a head revision, and a dirty flag. Tasks target a workspace ID, never a path.

## Cloud executors

The next iteration after the MVP.

- **Same binary,** run on a cloud machine with the same link protocol.
- **Workspaces are cloned** from the remote at the brief's base revision. Results come back as a pushed branch or a patch artifact. Work that needs your uncommitted local changes stays on the laptop executor.
- **Placement:** `delegate_task` gains `placement` (`local` or `cloud`). The default rule: local if the workspace is dirty or the brief needs laptop-only tools, otherwise either.
- **Connection direction is configurable.** A laptop Orchestrator can't accept connections from the cloud, so it dials the cloud executor and the protocol runs unchanged after the handshake. Once the Orchestrator itself moves to the cloud, every executor dials it.
- **The laptop can sleep.** Cloud workers keep going; results wait in the executor's journal and are delivered when the Orchestrator reconnects.
- **Open:** where they run (a VM, a container service, or a hosted sandbox), and how repository access and harness credentials are provisioned.
