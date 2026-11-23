# Plan

In priority order: work from the top. Design is in [DESIGN.md](DESIGN.md).

## MVP: local Orchestrator, local executor (Claude Code + Codex), shared queue

- [x] **M2 Orchestrator core** (~4 days)
  - [x] SQLite store: migrations, one writer task, WAL with `synchronous=FULL`
  - [x] Event log with the outbox rule and cursor replay
  - [x] Task state machine with compare-and-set transitions and property tests
  - [x] Client API over WebSocket: `session.attach`, `events.subscribe`, `task.*`, `decision.respond`
- [x] **M3 Local executor** (~4 days)
  - [x] Link handshake with a local token; workspace registry
  - [x] Command journal, `Duplicate` on redelivery, reconcile on reconnect
  - [x] Read ops: bounded, with the `run_readonly` allowlist
  - [x] Test: kill the executor mid-command → `OutcomeUnknown`, never re-run
- [ ] **M4 Queue** (~3 days)
  - [ ] Bands, in-band score, manual rank, `why` (property tests)
  - [ ] Proposals for fields you own; `expected_version` on edits
  - [ ] `queue.*` and `todo.*` API; `console queue` and `console todo`
- [x] **Decide: Orchestrator isolation** — share your Claude Code login
- [ ] **M5 Orchestrator agent** (~5 days)
  - [ ] `cc-host`: Claude Code over `stream-json`, one prompt in flight, `--resume` after restart
  - [ ] MCP tools: workspace (read-only), queue, work
  - [ ] Hooks: per-turn state injection; the inline budget with forced delegation
  - [ ] `delegate_task` with briefs, against a fake worker; turn records
  - [ ] Headline-first answers in the system prompt
- [ ] **M6 Toad bridge** (~3 days) — usable daily from here
  - [ ] `console acp`: the Orchestrator conversation, with the queue as plan entries
  - [ ] Worker sessions (`--task`) and `/attach`
  - [ ] Slash commands
- [x] **Decide: worker permission policy** — inherit your Claude Code and Codex settings
- [ ] **M7 Workers** (~6 days)
  - [ ] Codex ACP spike: MCP passthrough, permission requests, cancel, auth
  - [ ] `acp-host` in the executor: harness config, process groups, brief as the first prompt
  - [ ] Worker MCP tools: ask, report, complete, follow-up, blocked
  - [ ] Questions and permissions surface as decisions in the conversation
  - [ ] Patch capture; concurrency limits (1 per workspace, 2 total)
- [ ] **M8 Daily-driver hardening** (~3 days)
  - [ ] `console up`/`down`/`status` as launchd agents
  - [ ] Restart recovery end to end; `OutcomeUnknown` resolution flow
  - [ ] `console inspect`, `console logs`; end-to-end failure tests

## Right after the MVP: prove it works (discuss first)

- [ ] Define and quantify "quality work", and how to show the product gets it done faster
- [ ] Metrics to collect
- [ ] Instrumenting the product
- [ ] Verification framework (for example, simulations)

## Next: cloud executors

- [ ] Decide the host (VM, container service, or hosted sandbox) and how repos and credentials are provisioned
- [ ] Configurable dial direction for the link
- [ ] Workspaces cloned at the base revision; results as a branch or patch
- [ ] `placement` on `delegate_task`, with the local-if-dirty rule

## Then: inbound events

- [ ] `inbound_events` stream with per-consumer cursors
- [ ] GitHub poller (review requests, mentions)
- [ ] Triage consumer: rules, then a model call; one todo per thread
- [ ] Slack and email pollers
- [ ] Choose a broker and move to webhooks once the Orchestrator is in the cloud

## Later

- [ ] Progressive-disclosure digests (L0–L3) and learned detail preferences
- [ ] Delegation tuning: labels, `console eval delegation`
- [ ] Resume an interrupted worker's session instead of starting over
- [ ] Autonomous scheduler for the Orchestrator's own todos
- [ ] Terminals (`console term`)
- [ ] Cloud Orchestrator
- [ ] Slack and desktop clients; generated views
- [ ] Voice

## Done

- [x] **M0 Prototypes** — Toad, Claude Code worker over ACP, PTY reattach, and Claude Code as the Orchestrator all verified ([findings](spikes/README.md))
- [x] **M1 Skeleton** — crates, `proto` wire types with snapshot tests, config, the `console` binary, CI
