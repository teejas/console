# console

A persistent engineering assistant built around an Orchestrator: a conversational agent you stay in sync with through a shared priority queue, which delegates substantial work to coding agents running in your real development environment. It's written in Rust and runs locally today, and the same Orchestrator is designed to move to the cloud while Toad stays your local chat client.

## Directory structure

```
.
├── Cargo.toml                 # Rust workspace
├── research/
│   ├── industry-research.md   # landscape of existing agents, TUIs, and protocols
│   ├── architecture-design.md # target architecture and accepted product decisions
│   └── implementation-plan.md # v1 plan: milestones, contracts, risks
└── spikes/                    # M0 prototypes that validated the reused components
    ├── README.md              # findings and resulting plan changes
    ├── acp-stub-agent/        # stub ACP agent for probing Toad
    ├── acp-host-claude/       # Rust ACP client hosting Claude Code workers
    ├── mcp-stub/              # stub of the Orchestrator's MCP tool server
    ├── orchestrator-cc/       # Claude Code CLI as the Orchestrator (hooks, config)
    └── pty-reattach/          # PTY detach/reattach and headless TUI driver
```
