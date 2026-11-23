# console

A persistent engineering assistant built around an Orchestrator: a conversational agent you stay in sync with through a shared priority queue, which delegates work to Executor agents running in your real development environment or cloud environments. The Orchestrator can be deployed to a cloud environment itself and supports Slack, chat, and voice interfaces.

## Quick start

Requirements: Rust 1.100, Claude Code (logged in), Toad, and the worker adapters:

```sh
npm install -g @agentclientprotocol/claude-agent-acp@0.109.1 @agentclientprotocol/codex-acp@2.9.0
cargo install --path crates/console
```

Tell the executor which workspaces it owns in `~/.console/config.toml`:

```toml
[[executor.workspaces]]
name = "console"
path = "/path/to/console"
```

Start the Orchestrator and executor as launchd agents, then talk to the Orchestrator in Toad:

```sh
console up                      # start at login, restart on crash; `console down` to stop
toad acp "console acp"          # the Orchestrator conversation (slash commands: /queue /tasks /decisions …)
console status                  # services, workspaces, tasks, what's waiting on you
```

Everything is also available from the CLI: `console ask`, `console queue`, `console todo`, `console tasks`, `console decide`, `console logs -f`, `console inspect`, `console retry`, `console resolve`. See [DESIGN.md](DESIGN.md) for how it works and [PLAN.md](PLAN.md) for what's next.

## Directory structure

```
.
├── DESIGN.md                  # living architecture: each design choice and its purpose
├── PLAN.md                    # implementation plan as a prioritized checklist
├── Cargo.toml                 # Rust workspace
├── rust-toolchain.toml        # pinned toolchain
├── .github/workflows/ci.yml   # fmt, clippy (-D warnings), tests
├── crates/
│   ├── console/               # the binary: every role and user command
│   ├── proto/                 # wire types: IDs, events, client API, executor link
│   ├── config/                # layered config (defaults, TOML, CONSOLE_* env)
│   ├── store/                 # SQLite store and event log (M2)
│   ├── orchestrator/          # Orchestrator service (M2)
│   ├── client/                # client for the Orchestrator API (M2)
│   ├── executor/              # executor daemon (M3)
│   ├── queue/                 # shared priority queue (M4)
│   ├── cc-host/               # hosts the Orchestrator's Claude Code (M5)
│   ├── agent/                 # Orchestrator agent: hooks, tools, briefs (M5)
│   ├── acp-bridge/            # `console acp` bridge for Toad (M6)
│   └── acp-host/              # hosts Claude Code and Codex workers over ACP (M7)
├── docs/
│   ├── specs/                 # in-depth design docs, one per area of DESIGN.md
│   └── research/              # original inputs: industry research and architecture design
└── spikes/                    # M0 prototypes that validated the reused components
    ├── README.md              # findings and resulting plan changes
    ├── acp-stub-agent/        # stub ACP agent for probing Toad
    ├── acp-host-claude/       # Rust ACP client hosting Claude Code workers
    ├── mcp-stub/              # stub of the Orchestrator's MCP tool server
    ├── orchestrator-cc/       # Claude Code CLI as the Orchestrator (hooks, config)
    └── pty-reattach/          # PTY detach/reattach and headless TUI driver
```
