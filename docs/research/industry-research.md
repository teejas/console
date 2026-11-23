# Industry research: a persistent engineering assistant

**Research date:** November 22, 2026  
**Status:** Documentation-based assessment; candidate products have not been tested hands-on for this project.  
**Companion:** [Architecture design](architecture-design.md)

## 1. Executive assessment

The proposed assistant brings together capabilities that already exist across several products: coding agents, persistent gateways, local execution, organizational search, terminal clients, voice, and generated interfaces. The research did **not establish that one product delivers the entire requested experience as a coherent, deployable system**. That is a limit of the evidence, not a claim that no such product exists.

The strongest candidates and references are:

- **Hermes:** a strong first candidate to evaluate for the agent runtime and desktop experience. Its documented surface includes managed cloud hosting, remote execution backends, desktop extensions, programmatic interfaces, and desktop full-duplex voice. These features need to be tested together against the proposed architecture. [H1] [H3] [H5] [H7] [H8]
- **OpenClaw:** the clearest architectural reference for a remote gateway controlling a paired laptop node through an outbound connection. It also documents generated widgets in supported interfaces. [O1] [O2]
- **Cursor:** a relevant commercial benchmark for persistent project coordination and cloud orchestration with execution on user-managed machines. [C1] [C2]
- **Devin:** a relevant engineering workflow benchmark spanning desktop, CLI, and cloud, including a documented CLI-to-cloud handoff. [D1] [D2]
- **Toad, Pi TUI, OpenTUI, and OpenCode:** useful options for different layers of the terminal experience. They are not interchangeable: some are complete clients, some are UI libraries, and some depend on a particular agent server. [T1] [T2] [T4] [T5]

The recommended direction is to preserve the architecture independently of the first runtime choice: **one coordinator that can run locally or in the cloud, a persistent local executor, optional cloud workers, and multiple clients that attach to durable sessions and tasks.** Reuse existing components where their contracts fit; validate the integration before choosing a foundation.

## 2. What we are trying to build

The product is a persistent engineering assistant, not just a chat window around a coding model.

| Requirement | Intended experience |
|---|---|
| Plan and build | Turn a goal into a plan, make changes, run checks, and track the work to an outcome. |
| Walk through code before shipping | Explain the change, navigate relevant code and diffs, identify risks, and answer follow-up questions. |
| Review pull requests | Inspect changes in repository context, collect evidence, and produce review findings. |
| Track todos | Maintain durable tasks and dependencies across sessions and interfaces. |
| Answer across organizational knowledge | Use code, docs, Slack, and telemetry with source references, appropriate access, and freshness information. |
| Execute local commands | Provide both agent execution and a real human-operated terminal in the actual development environment. |
| Communicate through several interfaces | Support Slack, a TUI, desktop chat, and full-duplex voice. |
| Generate useful interfaces | Create task-specific views on the user's machine, preferably inside a native desktop host. |
| Hide routine infrastructure choices | Choose models, agent harnesses, and execution locations according to the task and configured policy. |
| Remember and persist | Retain preferences, tasks, conversations, workspaces, artifacts, and agent state beyond a chat session. |
| Manage agents | Create, supervise, interrupt, and recover agents running locally or in the cloud. |
| Integrate broadly | Reach external services through APIs, MCP servers, or installed CLIs. |
| Support both deployment modes | Run the same coordinator locally or in the cloud without redesigning the interfaces or executor. |

The latest deployment preference supersedes earlier local-only or cloud-only framing. We want **portable coordination**, with one active authority for an assistant at a time. Electron, a particular TUI library, and Hermes remain candidates rather than finalized selections.

## 3. Distinctions that clarify the market

### Client, coordinator, harness, model, and executor are separate choices

These roles are often bundled into one product, which makes feature comparisons misleading.

| Layer | Responsibility | Question to ask a candidate |
|---|---|---|
| Client | Chat, voice, terminal rendering, task views, input | Can it reconnect to an existing session and task? |
| Coordinator | Durable state, orchestration, task routing, policy | Can the same service run locally and remotely? |
| Harness | The agent loop and its tool/context behavior | Can it run as a supervised worker behind an adapter? |
| Model | Inference | Can providers change without changing the user workflow? |
| Executor | Files, commands, processes, device capabilities | Does it reach the real laptop environment? |
| State and artifacts | Memory, tasks, history, generated views, outputs | Who owns them, and what persists after a process ends? |

Consequently, “supports cloud” can mean managed hosting, a remotely hosted model, a remote shell, an ephemeral sandbox, or a full coordinator with persistent workers. Those are materially different capabilities.

### Why local execution matters here

The main reason is access to the actual working environment: uncommitted changes, installed dependencies, local services, existing CLI sessions, development databases, and laptop-attached capabilities. A fresh cloud checkout may be useful, but it is not automatically equivalent to that environment.

The coordinator's location is a different decision. Cloud coordination can keep Slack access and orchestration available while the laptop sleeps. Local coordination can simplify a personal installation and keep its authoritative state on the machine. Neither choice alone guarantees offline operation or local-only data processing: model calls, telemetry queries, and connectors have their own data flows.

### Native application versus native controls

The requested experience is an installed desktop application that can open useful task-specific interfaces. Electron can provide a desktop application using web technologies; it does not imply that every control is an operating-system-native widget. The architectural opportunity is a stable desktop host with generated views and a controlled action bridge. [U1]

## 4. Product landscape

“Not established” below means that the reviewed sources did not demonstrate the complete requirement. It is not a verified absence of the feature.

| Candidate | Documented strengths relevant to this project | What remains to establish |
|---|---|---|
| **Hermes** | Cloud hosting, execution backends, desktop extension SDK, programmatic interfaces, voice modes. [H1] [H3] [H5] [H7] [H8] | One portable authority with a paired laptop executor; coherent task continuity across all interfaces; complete engineering and organizational knowledge workflows. |
| **OpenClaw** | Remote gateway with paired laptop nodes; local device and execution capabilities; generated widgets. [O1] [O2] | Fit for the requested plan/build/review workflow, memory semantics, organizational retrieval, and full-duplex voice experience. |
| **Cursor** | Persistent project coordination and self-hosted execution connected to cloud agent orchestration. [C1] [C2] | User-deployable local coordinator, generated task-specific native views, and the full knowledge/voice requirements. |
| **Devin** | Desktop and CLI alongside cloud; local development access and cloud handoff. [D1] [D2] | Portable coordinator ownership and the complete generated-interface, memory, knowledge, and voice experience. |
| **OpenCode** | Terminal client with an explicit remote-server connection model. [T5] | Fit of its server semantics as the authoritative coordinator, or the cost of adapting its frontend to another service. |
| **Toad / Pi TUI / OpenTUI** | Reusable terminal frontend options at different abstraction levels. [T1] [T2] [T4] | Backend integration, durable sessions, authentication, remote transport, and product-specific views. |

### 4.1 Hermes: closest broad candidate to evaluate

**Cloud is a real deployment option.** Nous documents managed Hermes Cloud with a hosted agent, persistent workspace, dashboard, connected channels, and unattended work. That answers the narrow question of whether Hermes can be hosted in the cloud. It does not by itself establish access to the user's laptop. [H1]

**Remote execution is separately configurable.** Hermes documents local, Docker, SSH, Modal, Daytona, and Vercel terminal backends, with additional backend integrations. Persistence differs by backend. A snapshot or state synchronization feature should not be read as preservation of every process or automatic synchronization of an arbitrary development working tree. [H5]

**Multiple connections are not one distributed authority.** The desktop supports local, remote, SSH, and cloud connections. Its documentation describes sessions, memory, and files as belonging to their gateway/profile. A combined desktop roster is therefore different from one coordinator supervising multiple executors under one state model. Crossing gateway boundaries requires explicit integration. [H2]

**Generated desktop interfaces are unusually relevant.** The desktop plugin SDK supports agent-authored `plugin.js` extensions that can contribute pages, panes, navigation, commands, and hotkeys with hot reload. This is evidence for dynamic interfaces inside the installed application. It does not establish unrestricted creation of independent OS windows. The documented workspace-opening API opens a workspace tab. Remote backend code also does not automatically install its UI portion on the user's desktop. [H3]

The SDK's privileged plugin model is an implementation choice to evaluate. Our proposed architecture instead starts with generated artifacts and a constrained action bridge; adopting Hermes plugins would require deciding how those boundaries map. [H3]

**Desktop full-duplex voice is documented.** Hermes distinguishes a desktop full-duplex voice model mode from a speech-to-text/agent/text-to-speech pipeline. The voice model mode can delegate work to Hermes. Current documentation does not describe that same mode as available in the CLI/TUI. Its interruption behavior, latency, and continuity during long-running work still need hands-on evaluation. [H8]

**The stock TUI is not evidence of generic remote attachment.** Its documentation describes local gateway attachment using local connection tickets. An internal gateway URL setting should not be treated as a supported arbitrary remote coordinator client. [H6]

**There are integration seams.** Hermes exposes programmatic entry points including ACP, gateway RPC, and HTTP/SSE. Their capabilities differ. Launching an ACP agent process is not automatically the same operation as attaching to an already-running coordinator and its durable task state. Delegation-provider hooks also offer places to adapt execution policy. [H7] [H9]

**Assessment:** evaluate Hermes with a concrete cross-interface, laptop-execution prototype before deciding whether to extend it, wrap it, or use only selected components. Its feature breadth makes it promising; architectural fit is the remaining question.

### 4.2 OpenClaw: strongest reference for the gateway/laptop split

OpenClaw explicitly documents a gateway on another machine and a laptop acting as a paired node. The node connects outward to the gateway, allowing the gateway to call laptop capabilities without exposing inbound SSH. The documented capabilities include execution and device/browser-related operations. [O1]

That is directly relevant to our cloud-coordinator/local-executor mode. The same separation also helps reason about a local deployment: the authority and the machine that executes work are roles, even when colocated.

OpenClaw's `show_widget` documentation describes self-contained HTML widgets in its Control UI and supported native app transcripts, including isolated web views and a host bridge. This supports the “generate an interface inside a stable host” direction. These widgets should not be described as newly packaged standalone native applications. [O2]

**Assessment:** study the node connection, capability model, and widget lifecycle. Do not infer that those architectural features prove the entire requested engineering assistant experience.

### 4.3 Cursor: commercial reference for coordination and execution

Cursor Projects documents persistent coordination, planning, delegated agents, shared project context, and subscriptions to events such as Slack, PR, CI, and scheduled activity. The documentation also describes local agent work when a task needs the user's machine. This is relevant evidence that project coordination can span local and cloud execution. [C1]

Cursor's self-hosted machine documentation separates cloud planning/agent orchestration from a user-managed worker that edits files, runs commands, and accesses local capabilities. The worker uses outbound HTTPS. Although the checkout and machine credentials remain on that worker, selected files, outputs, diffs, screenshots, and tool results can be sent to the cloud service. [C2]

**Assessment:** use these workflows as a benchmark. “Self-hosted machine” here refers to execution and should not be equated with a fully locally deployable coordinator. [C2]

### 4.4 Devin: a broader surface than hosted coding alone

Devin's current desktop positioning includes local and cloud agents and ACP integration. Its CLI documents working with local repositories and shell access, plus a `/handoff` operation for continuing work in the cloud. [D1] [D2]

**Assessment:** compare task continuity and engineering workflows. Do not assume that a handoff transfers every local file, process, credential, or device capability, or that this implies a coordinator the user can deploy in either location.

## 5. Terminal interfaces: what can be reused?

The key question is whether we need an existing client to speak to our coordinator or a library from which to build our client.

| Option | What it provides | Integration consequence |
|---|---|---|
| **Toad** | An ACP-oriented terminal frontend with streaming Markdown, diffs, file selection, interactive shell, concurrent sessions, and resume behavior in its current README. | Fast candidate for a complete client. Requires a compatible agent adapter and a remote transport strategy. Its AGPL/commercial licensing should be evaluated for the intended distribution. [T1] |
| **Pi TUI** | Components for terminal editing, Markdown, selectors, scrolling, and related UI behavior. | Useful for a custom interface. We own authentication, network transport, task views, and durable session attachment. [T2] |
| **Pi RPC** | A coding-agent process controlled through stdin/stdout RPC. | Useful as a harness integration. It is not itself a general remote TUI for an arbitrary coordinator. [T3] |
| **OpenTUI** | A terminal UI library, including React and Solid integrations, used by OpenCode. | Useful for building a dedicated client with our own protocol and views. [T4] |
| **OpenCode** | A TUI that can connect to an OpenCode server; current CLI documentation includes `--server`. | Remote-capable, but coupled to OpenCode's backend contract. Supporting another coordinator requires a compatible server or frontend changes. [T5] |
| **Hermes TUI** | A terminal interface to its documented local gateway setup. | A useful native Hermes experience; generic remote attachment needs verification or additional integration. [H6] |

### ACP helps, but does not solve the entire client architecture

ACP v1 documents JSON-RPC over standard input/output as its established transport. Remote Streamable HTTP support is described as work being specified; custom transports are possible. We should therefore avoid assuming every ACP client can simply receive a cloud coordinator URL. [P1]

The community `acpremote` bridge demonstrates one approach: expose a remote WebSocket ACP endpoint through a local stdio process, including a documented Toad example. This is an adapter pattern, not proof of a universal finalized remote ACP standard. The bridge's process lifetime must also remain separate from the lifetime of our durable coordinator. [P2]

**Proposed direction:** define our own durable session/task API and consider an ACP adapter for compatible clients. Prototype Toad for speed, and compare it with a small client using Pi TUI or OpenTUI before committing. ACP should be an integration boundary, not an assumption that every product requirement fits inside one protocol.

## 6. Connecting Slack, voice, and generated interfaces

### Slack

Slack is a client and an event source. It should map an authorized user and thread to a coordinator session, receive progress, and link to richer task views. It should not own the task lifecycle.

Slack Socket Mode provides an outbound WebSocket connection for receiving events without a public HTTP endpoint. This makes a laptop-hosted coordinator practical while it is awake and connected. A cloud-hosted coordinator can use Socket Mode or an appropriate HTTP event adapter. [S1]

### Full-duplex voice

The requirement is simultaneous listening and speaking, with natural interruption. A push-to-talk transcription box does not satisfy it. The design needs a real-time media layer plus a durable task-control layer. A user interrupting spoken output may want to ask a follow-up while the build continues; stopping audio and cancelling work must be separate operations.

### Generated desktop views

Examples include a code walkthrough, an interactive diff review, a task board, a test-results view, or a live telemetry investigation. The assistant should generate the representation, while a stable installed host owns windows, authentication, rendering, and the action bridge.

Hermes plugins and OpenClaw widgets are useful evidence for this direction, with different extension boundaries. Neither should be treated as proof of our complete proposed view lifecycle. [H3] [O2]

## 7. Cross-cutting learnings

1. **A persistent daemon matters more than the chat surface.** Clients should be able to close, reconnect, and switch without losing the work they initiated.
2. **Execution placement must follow required capabilities and workspace state.** A laptop task cannot silently become a clean cloud checkout if its behavior depends on local changes or devices.
3. **A filesystem is not the same thing as memory.** Working files, durable preferences, task records, retrieved knowledge, and generated artifacts need distinct ownership and retention rules.
4. **Organizational knowledge is more than installing connectors.** Useful answers need source access checks, retrieval quality, freshness, citations, and appropriate handling of live telemetry queries.
5. **Model and harness abstraction needs capability negotiation.** Different runtimes expose different tools, context limits, interruption semantics, and session formats. A shared adapter cannot honestly promise perfect interchangeability.
6. **Multiple agents need supervision.** The system must account for task ownership, budgets, workspace conflicts, cancellation, and uncertain outcomes after disconnects.
7. **Generated interfaces should remain views over durable work.** A generated screen should not become a separate source of task truth.
8. **Local and cloud are deployment options, not separate products.** The contracts should remain stable when the coordinator moves.

These are design conclusions from the requirements and product comparison. They are not claims that every candidate already implements them.

## 8. What to validate before selecting a foundation

| Experiment | What it would establish |
|---|---|
| Submit in Slack, inspect in the TUI, continue in desktop chat | Shared identity, task continuity, and replay across interfaces. |
| Ask for a change requiring an uncommitted local file and a running local service | Genuine access to the actual development environment. |
| Close the initiating client while an operation runs | Separation of client and task lifetimes. |
| Disconnect the executor after a command is accepted | Reconciliation without blindly repeating an action. |
| Move the same coordinator configuration from local to cloud | Real deployment portability and state ownership. |
| Run an isolated cloud task and return a patch to the laptop | Explicit workspace transfer and safe integration of results. |
| Generate a review interface with working controls | The complete artifact, renderer, action, and event lifecycle. |
| Interrupt spoken output while a build continues | Full-duplex behavior and separate speech/task cancellation. |
| Ask a question spanning code, docs, Slack, and telemetry | Retrieval quality, access boundaries, citations, and freshness. |
| Correct a working preference and revisit it later | Inspectable, useful memory rather than uncontrolled transcript accumulation. |

Evaluation should record which features are built in, which require extensions, and which would require us to own substantial infrastructure. The initial goal is one complete workflow across the boundaries, not a large collection of disconnected demos.

## 9. Recommended build-versus-reuse position

**Own the product contracts:** durable tasks and sessions, workspace identity, executor capabilities, cross-interface continuity, generated-view lifecycle, memory ownership, and routing policy.

**Evaluate reuse first:** Hermes as a runtime/desktop foundation; OpenClaw as a gateway/node reference or candidate; an existing coding harness; Toad or a TUI library; an established desktop shell; existing real-time voice and connector infrastructure.

**Keep the first implementation replaceable:** a runtime or frontend should sit behind a boundary that lets the product retain its tasks, state, and execution semantics when that component changes. This does not mean building every abstraction in advance. It means proving the boundaries with one useful end-to-end workflow before expanding.

The companion architecture records the accepted deployment direction and distinguishes it from these implementation proposals.

## 10. Sources and evidence notes

All links below are primary product documentation or project repositories consulted for this brief. Capabilities may change after the research date. No benchmark results or production reliability claims are implied.

- [H1 — Nous: Hermes Cloud][H1]
- [H2 — Hermes: multiple desktop connections][H2]
- [H3 — Hermes: desktop plugin SDK][H3]
- [H4 — Hermes: desktop application][H4]
- [H5 — Hermes: configuration and terminal backends][H5]
- [H6 — Hermes: TUI][H6]
- [H7 — Hermes: programmatic integration][H7]
- [H8 — Hermes: voice mode][H8]
- [H9 — Hermes: delegation provider plugins][H9]
- [O1 — OpenClaw: remote gateways and nodes][O1]
- [O2 — OpenClaw: generated widgets][O2]
- [C1 — Cursor: Projects][C1]
- [C2 — Cursor: self-hosted cloud-agent machines][C2]
- [D1 — Devin Desktop][D1]
- [D2 — Devin CLI][D2]
- [T1 — Toad: current README][T1]
- [T2 — Pi: TUI package][T2]
- [T3 — Pi: coding-agent RPC][T3]
- [T4 — OpenTUI][T4]
- [T5 — OpenCode: CLI and server connection][T5]
- [P1 — ACP v1: transports][P1]
- [P2 — acpremote: WebSocket-to-stdio bridge][P2]
- [S1 — Slack: Socket Mode][S1]
- [U1 — Electron documentation][U1]

[H1]: https://portal.nousresearch.com/cloud
[H2]: https://hermes-agent.nousresearch.com/docs/user-guide/multi-connection-desktop
[H3]: https://hermes-agent.nousresearch.com/docs/developer-guide/desktop-plugin-sdk
[H4]: https://hermes-agent.nousresearch.com/docs/user-guide/desktop
[H5]: https://hermes-agent.nousresearch.com/docs/user-guide/configuration
[H6]: https://hermes-agent.nousresearch.com/docs/user-guide/tui
[H7]: https://hermes-agent.nousresearch.com/docs/developer-guide/programmatic-integration
[H8]: https://hermes-agent.nousresearch.com/docs/user-guide/features/voice-mode
[H9]: https://hermes-agent.nousresearch.com/docs/developer-guide/delegation-provider-plugin
[O1]: https://docs.openclaw.ai/help/faq/remote-gateways-and-nodes.md
[O2]: https://docs.openclaw.ai/tools/show-widget.md
[C1]: https://cursor.com/docs/agent/projects
[C2]: https://cursor.com/docs/cloud-agent/self-hosted
[D1]: https://devin.ai/desktop
[D2]: https://devin.ai/cli
[T1]: https://raw.githubusercontent.com/batrachianai/toad/main/README.md
[T2]: https://github.com/earendil-works/pi/blob/main/packages/tui/README.md
[T3]: https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/rpc.md
[T4]: https://github.com/anomalyco/opentui
[T5]: https://opencode.ai/docs/cli/
[P1]: https://agentclientprotocol.com/protocol/v1/transports
[P2]: https://github.com/vcoderun/acpkit/blob/main/packages/transports/acpremote/README.md
[S1]: https://docs.slack.dev/apis/events-api/using-socket-mode/
[U1]: https://www.electronjs.org/docs/latest
