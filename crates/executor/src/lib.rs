//! The executor daemon: Orchestrator link, journal, workspaces, read-only ops, worker host (M3).
//!
//! See `docs/specs/executors.md`.

pub mod journal;
pub mod readops;
pub mod workspaces;

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use console_config::Config;
use console_proto::link::{
    Capability, CommandOutcome, CommandStatus, ExecutorMsg, JournalEntry, LINK_PROTOCOL_VERSION,
    Op, OrchestratorMsg,
};
use console_proto::{AttemptId, AuthorityEpoch, CommandId, ExecutorId};
use futures::{SinkExt as _, StreamExt as _};
use tokio::io::AsyncReadExt as _;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_util::sync::CancellationToken;

use crate::journal::Journal;
use crate::workspaces::{OUTPUT_CAP, Registry, kill_group, run_capture, term_group};

/// Shared executor state; survives reconnects.
#[derive(Debug)]
pub struct Executor {
    pub id: ExecutorId,
    pub config: Config,
    pub journal: Journal,
    pub registry: Registry,
    out: Mutex<Option<mpsc::UnboundedSender<String>>>,
    running: Mutex<HashMap<CommandId, CancellationToken>>,
    tasks: Mutex<Vec<tokio::task::AbortHandle>>,
}

/// The journal file inside the data directory.
#[must_use]
pub fn journal_path(config: &Config) -> PathBuf {
    config.data_dir.join("executor").join("journal.db")
}

impl Executor {
    /// Opens the journal, marks commands that died with a previous process as interrupted,
    /// and loads the workspace registry.
    pub async fn open(config: Config) -> anyhow::Result<Arc<Self>> {
        let journal = Journal::open(&journal_path(&config)).await?;
        let interrupted = journal.interrupt_unfinished().await?;
        if interrupted > 0 {
            tracing::warn!(
                interrupted,
                "commands were running when the executor last stopped"
            );
        }
        let id = journal.executor_id().await?;
        let registry = Registry::load(&config.executor.workspaces, &journal).await?;
        Ok(Arc::new(Self {
            id,
            config,
            journal,
            registry,
            out: Mutex::new(None),
            running: Mutex::new(HashMap::new()),
            tasks: Mutex::new(Vec::new()),
        }))
    }

    /// Sends a message on the current connection. Returns false when disconnected.
    pub fn send(&self, msg: &ExecutorMsg) -> bool {
        let Ok(text) = serde_json::to_string(msg) else {
            return false;
        };
        self.out
            .lock()
            .ok()
            .and_then(|o| o.as_ref().map(|tx| tx.send(text).is_ok()))
            .unwrap_or(false)
    }

    fn capabilities(&self) -> Vec<Capability> {
        let mut caps = vec![Capability::Shell, Capability::Git];
        for harness in self.config.executor.harnesses.keys() {
            caps.push(Capability::Worker {
                harness: harness.clone(),
            });
        }
        caps
    }

    /// Connects, serves, and reconnects with backoff until `shutdown` resolves.
    pub async fn run_until(self: Arc<Self>, shutdown: impl Future<Output = ()>) {
        let mut shutdown = std::pin::pin!(shutdown);
        let mut backoff = Duration::from_millis(250);
        loop {
            tokio::select! {
                () = &mut shutdown => return,
                r = self.clone().connect_once() => {
                    match r {
                        Ok(()) => {
                            tracing::info!("disconnected from the Orchestrator");
                            backoff = Duration::from_millis(250);
                        }
                        Err(e) => tracing::warn!(error = %e, "Orchestrator link failed"),
                    }
                }
            }
            tokio::select! {
                () = &mut shutdown => return,
                () = tokio::time::sleep(backoff) => {}
            }
            backoff = (backoff * 2).min(Duration::from_secs(10));
        }
    }

    async fn connect_once(self: Arc<Self>) -> anyhow::Result<()> {
        let token = self.config.token()?;
        let mut request = self
            .config
            .executor
            .orchestrator_url
            .as_str()
            .into_client_request()?;
        request.headers_mut().insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {token}"))?,
        );
        let (ws, _) = tokio_tungstenite::connect_async(request).await?;
        let (mut sink, mut stream) = ws.split();

        let hello = ExecutorMsg::Hello {
            executor_id: self.id,
            name: self.config.executor.name.clone(),
            protocol: LINK_PROTOCOL_VERSION,
            max_epoch_seen: self.journal.max_epoch_seen().await?,
            capabilities: self.capabilities(),
            workspaces: self.registry.infos().await,
            journal: self.journal.entries().await?,
        };
        sink.send(Message::Text(serde_json::to_string(&hello)?.into()))
            .await?;

        // The first reply decides whether this connection is accepted.
        let first = loop {
            match stream.next().await {
                Some(Ok(Message::Text(t))) => break serde_json::from_str::<OrchestratorMsg>(&t)?,
                Some(Ok(Message::Close(_))) | None => anyhow::bail!("closed before Welcome"),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
            }
        };
        match first {
            OrchestratorMsg::Welcome { epoch, .. } => {
                self.journal.see_epoch(epoch).await?;
                tracing::info!(executor = %self.id, epoch = epoch.0, "connected to the Orchestrator");
            }
            OrchestratorMsg::Reject { reason } => anyhow::bail!("rejected: {reason}"),
            other => anyhow::bail!("expected Welcome, got {other:?}"),
        }

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
        let writer = tokio::spawn(async move {
            while let Some(m) = out_rx.recv().await {
                if sink.send(Message::Text(m.into())).await.is_err() {
                    break;
                }
            }
        });
        if let Ok(mut out) = self.out.lock() {
            *out = Some(out_tx);
        }
        if let Ok(mut tasks) = self.tasks.lock() {
            tasks.push(writer.abort_handle());
        }

        // Results that finished while disconnected.
        for u in self.journal.undelivered().await? {
            if self.send(&ExecutorMsg::Result {
                command_id: u.command_id,
                outcome: u.outcome,
            }) {
                self.journal.mark_delivered(u.command_id).await?;
            }
        }

        while let Some(msg) = stream.next().await {
            let text = match msg? {
                Message::Text(t) => t,
                Message::Close(_) => break,
                _ => continue,
            };
            match serde_json::from_str::<OrchestratorMsg>(&text) {
                Ok(m) => self.clone().handle(m).await,
                Err(e) => tracing::warn!(error = %e, "bad message from the Orchestrator"),
            }
        }
        if let Ok(mut out) = self.out.lock() {
            *out = None;
        }
        writer.abort();
        Ok(())
    }

    async fn handle(self: Arc<Self>, msg: OrchestratorMsg) {
        match msg {
            OrchestratorMsg::Dispatch {
                command_id,
                attempt_id,
                epoch,
                op,
            } => self.dispatch(command_id, attempt_id, epoch, op).await,
            OrchestratorMsg::Cancel { command_id, .. } => {
                let token = self
                    .running
                    .lock()
                    .ok()
                    .and_then(|r| r.get(&command_id).cloned());
                match token {
                    Some(t) => t.cancel(),
                    None => {
                        let status = self
                            .journal
                            .status(command_id)
                            .await
                            .unwrap_or(CommandStatus::Unknown);
                        tracing::debug!(command = %command_id, ?status, "cancel for a command that isn't running");
                    }
                }
            }
            OrchestratorMsg::Reconcile { command_ids } => {
                let mut entries = Vec::new();
                for cid in command_ids {
                    let status = self
                        .journal
                        .status(cid)
                        .await
                        .unwrap_or(CommandStatus::Unknown);
                    entries.push(JournalEntry {
                        command_id: cid,
                        attempt_id: AttemptId::generate(),
                        status,
                        updated_at: chrono::Utc::now(),
                    });
                }
                self.send(&ExecutorMsg::ReconcileReport { entries });
            }
            OrchestratorMsg::ReadOp {
                request_id,
                op,
                deadline_ms,
            } => {
                let this = self.clone();
                tokio::spawn(async move {
                    let deadline = Duration::from_millis(deadline_ms.clamp(100, 120_000));
                    let result = readops::run(
                        &this.registry,
                        &this.config.executor.readonly_allowlist,
                        op,
                        deadline,
                    )
                    .await;
                    this.send(&ExecutorMsg::ReadOpResult { request_id, result });
                });
            }
            OrchestratorMsg::DecisionResponse { .. } | OrchestratorMsg::McpResult { .. } => {
                tracing::debug!("worker messages arrive with workers (M7)");
            }
            OrchestratorMsg::Welcome { .. } | OrchestratorMsg::Reject { .. } => {}
        }
    }

    async fn dispatch(
        self: Arc<Self>,
        cid: CommandId,
        aid: AttemptId,
        epoch: AuthorityEpoch,
        op: Op,
    ) {
        let max = self
            .journal
            .max_epoch_seen()
            .await
            .unwrap_or(AuthorityEpoch(0));
        if epoch < max {
            self.send(&ExecutorMsg::Result {
                command_id: cid,
                outcome: CommandOutcome::Failed {
                    error: format!("stale epoch {} < {}", epoch.0, max.0),
                },
            });
            return;
        }
        match self.journal.accept(cid, aid, &op).await {
            Ok(Some(status)) => {
                self.send(&ExecutorMsg::Duplicate {
                    command_id: cid,
                    status,
                });
                return;
            }
            Ok(None) => {}
            Err(e) => {
                tracing::error!(error = %e, "journal write failed; refusing the command");
                return;
            }
        }
        self.send(&ExecutorMsg::Accepted { command_id: cid });
        let token = CancellationToken::new();
        if let Ok(mut running) = self.running.lock() {
            running.insert(cid, token.clone());
        }
        let this = self.clone();
        let handle = tokio::spawn(async move {
            let _ = this.journal.set_status(cid, CommandStatus::Running).await;
            let outcome = this.execute(op, token).await;
            if let Ok(mut running) = this.running.lock() {
                running.remove(&cid);
            }
            this.finish(cid, outcome).await;
        });
        if let Ok(mut tasks) = self.tasks.lock() {
            tasks.retain(|t| !t.is_finished());
            tasks.push(handle.abort_handle());
        }
    }

    /// Drops every in-flight command without recording an outcome, killing their child
    /// processes, as a process crash would. Used by tests; a real crash needs no help.
    pub fn abort_all(&self) {
        if let Ok(mut out) = self.out.lock() {
            *out = None;
        }
        if let Ok(mut tasks) = self.tasks.lock() {
            for t in tasks.drain(..) {
                t.abort();
            }
        }
    }

    async fn finish(&self, cid: CommandId, outcome: CommandOutcome) {
        if let Err(e) = self.journal.finish(cid, &outcome).await {
            tracing::error!(error = %e, command = %cid, "could not journal the result");
        }
        if self.send(&ExecutorMsg::Result {
            command_id: cid,
            outcome,
        }) {
            let _ = self.journal.mark_delivered(cid).await;
        }
    }

    async fn execute(&self, op: Op, cancel: CancellationToken) -> CommandOutcome {
        match op {
            Op::Exec {
                workspace_id,
                argv,
                timeout_ms,
            } => {
                let Some(ws) = self.registry.get(workspace_id) else {
                    return CommandOutcome::Failed {
                        error: "unknown workspace on this executor".into(),
                    };
                };
                exec(&ws.root, &argv, Duration::from_millis(timeout_ms), cancel).await
            }
            Op::CapturePatch {
                workspace_id,
                base_rev,
            } => {
                let Some(ws) = self.registry.get(workspace_id) else {
                    return CommandOutcome::Failed {
                        error: "unknown workspace on this executor".into(),
                    };
                };
                capture_patch(&ws.root, &base_rev).await
            }
            Op::StartWorker { .. } | Op::WorkerPrompt { .. } => CommandOutcome::Failed {
                error: "workers are not supported by this executor yet".into(),
            },
        }
    }
}

/// Runs a non-agent command in its own process group; cancel or timeout stops the group.
async fn exec(
    dir: &std::path::Path,
    argv: &[String],
    timeout: Duration,
    cancel: CancellationToken,
) -> CommandOutcome {
    let Some((program, args)) = argv.split_first() else {
        return CommandOutcome::Failed {
            error: "empty command".into(),
        };
    };
    let mut child = match tokio::process::Command::new(program)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return CommandOutcome::Failed {
                error: format!("{program}: {e}"),
            };
        }
    };
    let pid = child.id();
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let collect = async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        if let Some(s) = stdout.as_mut() {
            let _ = s.take(OUTPUT_CAP as u64).read_to_end(&mut out).await;
            let _ = tokio::io::copy(s, &mut tokio::io::sink()).await;
        }
        if let Some(s) = stderr.as_mut() {
            let _ = s.take(16 * 1024).read_to_end(&mut err).await;
            let _ = tokio::io::copy(s, &mut tokio::io::sink()).await;
        }
        (out, err)
    };
    let waited = async { tokio::join!(collect, child.wait()) };
    tokio::select! {
        ((out, err), status) = waited => match status {
            Ok(s) if s.success() => CommandOutcome::Succeeded {
                summary: Some(String::from_utf8_lossy(&out).into_owned()),
                patch: None,
            },
            Ok(s) => CommandOutcome::Failed {
                error: format!("exit {}: {}", s.code().map_or_else(|| "signal".into(), |c| c.to_string()), String::from_utf8_lossy(&err).trim()),
            },
            Err(e) => CommandOutcome::Failed { error: e.to_string() },
        },
        () = cancel.cancelled() => {
            if let Some(pid) = pid {
                term_group(pid);
                tokio::time::sleep(Duration::from_millis(500)).await;
                kill_group(pid);
            }
            CommandOutcome::Cancelled
        }
        () = tokio::time::sleep(timeout) => {
            if let Some(pid) = pid {
                kill_group(pid);
            }
            CommandOutcome::Failed { error: format!("timed out after {}s", timeout.as_secs()) }
        }
    }
}

async fn capture_patch(dir: &std::path::Path, base_rev: &str) -> CommandOutcome {
    if base_rev.starts_with('-') {
        return CommandOutcome::Failed {
            error: format!("invalid revision {base_rev:?}"),
        };
    }
    let mut patch =
        match run_capture(dir, "git", &["diff", base_rev], Duration::from_secs(60)).await {
            Ok(c) if c.ok => c.stdout,
            Ok(c) => return CommandOutcome::Failed { error: c.stderr },
            Err(e) => {
                return CommandOutcome::Failed {
                    error: e.to_string(),
                };
            }
        };
    // Untracked files, diffed against nothing, without touching the index.
    if let Ok(untracked) = run_capture(
        dir,
        "git",
        &["ls-files", "--others", "--exclude-standard"],
        Duration::from_secs(30),
    )
    .await
    {
        for file in untracked.stdout.lines().filter(|l| !l.is_empty()) {
            if let Ok(c) = run_capture(
                dir,
                "git",
                &["diff", "--no-index", "--", "/dev/null", file],
                Duration::from_secs(30),
            )
            .await
            {
                patch.push_str(&c.stdout);
            }
        }
    }
    CommandOutcome::Succeeded {
        summary: Some(if patch.is_empty() {
            "no changes".into()
        } else {
            "patch captured".into()
        }),
        patch: Some(patch),
    }
}

/// Runs the executor until interrupted.
pub async fn run(config: Config) -> anyhow::Result<()> {
    let exec = Executor::open(config).await?;
    tracing::info!(executor = %exec.id, workspaces = exec.registry.all().count(), "executor starting");
    exec.run_until(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await;
    Ok(())
}
