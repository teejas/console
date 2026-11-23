# M0 prototype findings

**Date:** November 22, 2026
**Environment:** macOS arm64, Rust 1.100.0, Claude Code 2.1.330, Toad 0.6.20 (Python 3.14 via uv), `@agentclientprotocol/claude-agent-acp` 0.109.1, `agent-client-protocol` 5.5.1, `rmcp` 3.11.0, `portable-pty` 0.9.0, `vt100` 0.18.1.

| Prototype | Crate / folder | Result |
|---|---|---|
| (a) Stub Orchestrator agent under Toad | `acp-stub-agent` | ✅ Everything needed works; two UX notes |
| (b) Claude Code worker via the ACP adapter, from Rust, with MCP attached | `acp-host-claude`, `mcp-stub` | ✅ Works; two operational findings |
| (c) PTY ownership and reattach | `pty-reattach` | ✅ Works; also serves as a headless TUI driver |
| (d) Claude Code as the Orchestrator | `orchestrator-cc` | ✅ Works through the CLI's stream-json mode; isolation needs a decision |

## (a) Toad

How it was tested: `pty-reattach` drove `toad acp <stub>` in a 140×45 PTY and captured screens.

- **Protocol:** Toad speaks ACP **v1** (`PROTOCOL_VERSION = 1`). The bridge stays v1, even though the Rust crate also offers v2 (preview), which has resume cursors, idempotency keys, and idle-state updates.
- **Out-of-turn updates render.** `agent_message_chunk` and `plan` updates sent with no prompt in flight appear immediately. Both cases worked: right after `session/new`, and 3 s after a turn had ended (`/notify 3`). Toad doesn't check whether a prompt is active. This means background digests can be pushed live, and the notification fallback isn't needed.
- **Plan entries render in a sidebar "Plan" panel** and also inline, with status icons (👉 in progress, ✔ completed, • pending). It works as the queue mirror; P0–P3 labels go in the entry text.
- **Replay during `session/new` renders.** History sent before the `session/new` response shows up as the transcript. So `console acp` can open the single long-lived Orchestrator conversation on every launch, with no dependence on Toad's own session store or resume UI.
- **Tool calls render collapsed** (`▶ 🔧 title ✔`). This suits inline lookups and "Delegated → T-123" entries.
- **Slash commands** from `available_commands_update` appear in a `/` completion menu. In that menu, Enter *inserts* the command and a second Enter sends it. Prompts arrive at the agent as plain text (`"/notify 3"`), and the bridge parses them.
- **Markdown renders**, including lists, bold, and code.
- **UX note:** consecutive `agent_message_chunk`s are concatenated, both across replayed turns and when an out-of-turn message follows a reply. The bridge must end every logical message with a blank line or separator.
- **UX note:** the `^s` Sessions screen lists only sessions open in the running app. Opening a worker session for `/attach` in a **separate Toad instance** (`toad acp "console acp --task T-123"`) works. Switching between multiple sessions inside one Toad window was not tested.
- **Side effect:** Toad's first run wrote `~/.config/toad/toad.json`.

## (b) Claude Code worker via the ACP adapter

- `npx -y @agentclientprotocol/claude-agent-acp` initializes and advertises `loadSession=true`.
  - **Startup:** about 5 s cold through npx, about 0.6 s warm. Pin and install the adapter (no npx at runtime).
- `session/new` with a stdio MCP server works. The worker wrote `NOTES.md` into the real workspace (a git repo) and called our MCP `queue_list` tool. The turn finished in 14.5 s cold.
- Update stream: `tool_call` arrives with kind (`Edit`, `Execute`, `Other`), then `tool_call_update`s and `usage_update`s. There's also a `session_info_update` that auto-titles the session, which is useful for task titles.
- **No permission requests were raised** for Write or Bash. Workers inherit your Claude Code settings: permission mode, skills (about 80 slash commands), and memory. Starting context was about 68k tokens. That's reasonable for workers, which act as your coding agent, but they need an explicit permission policy before they run unattended.
- **Cancel works.** `session/cancel` during a long foreground command returned `stop_reason=Cancelled` immediately, marked the tool call `Failed`, and killed the process.
- ⚠️ **End of turn doesn't mean done.**
  - Asked to run `sleep 30 && …`, the worker *backgrounded* it and ended its turn at 7 s ("I'll tell you when it finishes").
  - Asked to run a long sleep in the foreground, it declined.
  - This confirms the plan's explicit `complete(summary, evidence)` tool. The brief must tell the worker that ending its turn without calling `complete` means "needs input".
- ⚠️ **Orphaned processes outlive the worker.** The backgrounded `sleep 30` finished and wrote `SLEPT.txt` *after* the adapter process had exited. The executor must start each worker in its own process group or session and kill the group when the task ends or is cancelled. It should also record the group's processes in the journal.
- The adapter supports **MCP-over-ACP** (`"type": "acp"`), and the Rust crate has `unstable_mcp_over_acp`. The executor could serve the worker's Orchestrator tools over the same ACP connection, with no separate MCP process. Evaluate this in M7.

## (c) PTY reattach

- A zsh session in a PTY was detached, written to while detached, then reattached.
  - The owner side never stopped parsing the output into `vt100`.
  - The new client received `contents_formatted()` (316 bytes), replayed it into a fresh parser, and got a **matching screen and cursor**.
- The same harness drives Textual TUIs (Toad) reliably for scripted UI checks; it's reusable for e2e tests.
- API notes for vt100 0.18: `Parser::new` takes `NonZeroU16` rows and columns, and `contents_formatted()` returns a `String`.

## (d) Claude Code as the Orchestrator

How it was driven: `claude -p --input-format stream-json --output-format stream-json`. The flags were:

```
--model claude-opus-5-5 --effort medium
--tools "" --strict-mcp-config --mcp-config <ours>
--setting-sources "" --settings <ours: permissions + hooks>
--system-prompt <ours> --permission-prompts none
```

To reproduce: `spikes/orchestrator-cc/run.sh` (about $0.08 per run).

- **Built-in tools are off.** `--tools ""` left only our three MCP tools, and the model confirmed it could see only those.
- **Inline-or-delegate behaved as intended** on the test turns:

  | Turn | Behavior |
  |---|---|
  | Queue question | Inline `queue_list` |
  | Code question | Inline `ws_read_file` |
  | "Fix … and add a test" | `delegate_task`, with 4 acceptance criteria |
  | Read 5 files | Escalated to `delegate_task` once the budget was hit |

- **Budget guardrail works.** A `PreToolUse` hook denying calls past the budget showed up to the model as a tool error with our reason, and the call never reached the MCP server. The model then **delegated, carrying its partial findings into the brief** ("Findings so far: a.rs and b.rs are…").
- **State injection works.** `UserPromptSubmit` `additionalContext` was visible to the model (it read back the `PAPAYA-42` sentinel).
- **Resume works.** After the process exited, `--resume <session_id>` with the same flags restored the full context in about 4 s, and the hooks fired again.
- ⚠️ **Turns merge when prompts are queued.** Five stream-json user messages piped at once were answered as *one* turn ("Here are answers to all five of your messages"). The service must send one prompt and wait for its `result` message before sending the next. That's also when the per-turn tool counter resets.
- **Cost and latency:** the 5-turn run took 24 s of API time for 10 model calls and cost about $0.076. Cache: about 8.9k tokens read, 4.9k written. The replaced system prompt keeps the prefix small.
- **One-shot digests** (`claude -p --json-schema … --tools "" --effort low`):
  - Validated structured output, which correctly set `needs_you=true` for a pending worker question.
  - About 1.4k prompt tokens.
  - About 3–3.6 s API time and about 7 s wall time; the gap is Claude Code startup.
  - $0.0185 uncached, $0.0074 cached.
  - To cut latency, a long-lived digest process in stream-json mode could avoid the per-call startup.
- **Isolation is partial.**
  - `--setting-sources ""` plus `--strict-mcp-config` plus a replaced system prompt keeps your settings, hooks, and MCP servers out.
  - Your **skills, built-in plugins, and subagent definitions are still listed** in `init`. They're inert because no tools are enabled.
  - An auto-memory path is set, under `~/.claude/projects/<cwd-slug>/memory/`.
  - A separate `CLAUDE_CONFIG_DIR` gives full isolation, but it **doesn't share your login** ("Not logged in"), so it needs a one-time `/login` for that directory.
- **ACP adapter vs. CLI for the Orchestrator:** the CLI's flags give direct, verified control over tools, MCP, settings, hooks, system prompt, model, effort, and resume. The adapter is the right fit for *workers*, which need rich tool-call updates, permissions, and cancel. **Recommendation:** host the Orchestrator through the CLI's stream-json mode and workers through the ACP adapter. The two hosts share only process management.

## Changes to the implementation plan

1. Orchestrator hosting: Claude Code CLI in `stream-json` mode, a `cc-host` module. Workers keep using the ACP adapter (`acp-host`).
2. The Orchestrator service serializes prompts: one in flight, and the next is sent after `result`.
3. The bridge relies on replay during `session/new` for the single conversation, and adds message separators. Out-of-turn digests are pushed live, so notifications are optional.
4. The executor runs workers in their own process group or session, kills the group when a task ends or is cancelled, and pins the adapter version (no npx at runtime).
5. Briefs state the `complete` contract explicitly. Ending a turn without calling `complete` means "needs input".
6. The worker permission policy must be set explicitly, not inherited, before workers run unattended (M7).
7. Open decision: Orchestrator isolation. Either the shared login (inert skills and plugins) or a dedicated `CLAUDE_CONFIG_DIR` with a one-time login.
