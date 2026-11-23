//! The Orchestrator service: client API server, task engine, dispatcher, executor hub (M2).
//!
//! See `docs/specs/topology.md`.

pub mod agent_runner;
pub mod api;
pub mod engine;
pub mod hub;
pub mod inspect;
pub mod mcp;
pub mod queue_ops;
pub mod server;
pub mod task_machine;
pub mod work;
pub mod worker_link;

use std::path::PathBuf;
use std::sync::Arc;

use console_config::Config;
use console_proto::api::TodoView;
use console_proto::link::{CommandStatus, ExecutorMsg};
use console_proto::rpc::RpcError;
use console_proto::{ExecutorId, SessionId};
use console_store::{CommandRecord, DecisionRecord, Store, TaskRecord};
use serde_json::Value;
use tokio::net::TcpListener;

use crate::server::Conn;

/// Internal method the `console hook prompt-submit` command calls.
pub const HOOK_PROMPT_SUBMIT: &str = "hook.prompt_submit";
/// Internal method the `console hook pre-tool` command calls.
pub const HOOK_PRE_TOOL: &str = "hook.pre_tool";

/// How many queue items attach snapshots and plan mirrors show.
pub const QUEUE_TOP: usize = 8;

/// Shared service state.
#[derive(Debug)]
pub struct App {
    pub store: Store,
    pub config: Config,
    pub token: String,
    pub orchestrator_session: SessionId,
    pub hub: hub::Hub,
    pub agent: agent_runner::AgentRunner,
}

impl App {
    /// Opens the store and prepares the service, without binding a socket.
    pub async fn new(config: Config, store: Store, token: String) -> anyhow::Result<Arc<Self>> {
        store
            .write(async |tx| tx.all_executors_offline().await)
            .await?;
        let orchestrator_session = store.orchestrator_session().await?.id;
        Ok(Arc::new(Self {
            store,
            config,
            token,
            orchestrator_session,
            hub: hub::Hub::default(),
            agent: agent_runner::AgentRunner::default(),
        }))
    }

    /// Queue, workspace, and later-milestone methods.
    pub(crate) async fn dispatch_more(
        self: &Arc<Self>,
        _conn: &Conn,
        method: &str,
        p: Value,
    ) -> api::ApiResult<Value> {
        use console_proto::api::{
            Method as _, ProposalList, ProposalResolve, QueueList, TodoAdd, TodoMove, TodoUpdate,
            WorkspaceList,
        };
        use console_queue::Actor;
        match method {
            QueueList::NAME => {
                let p = api::params::<QueueList>(p)?;
                api::result::<QueueList>(queue_ops::list(self, p.include_done).await?)
            }
            TodoAdd::NAME => {
                let p = api::params::<TodoAdd>(p)?;
                api::result::<TodoAdd>(queue_ops::add(self, p, Actor::User).await?)
            }
            TodoUpdate::NAME => {
                let p = api::params::<TodoUpdate>(p)?;
                api::result::<TodoUpdate>(queue_ops::update(self, p, Actor::User, "").await?)
            }
            TodoMove::NAME => {
                let p = api::params::<TodoMove>(p)?;
                api::result::<TodoMove>(
                    queue_ops::move_todo(self, p.todo_id, p.before, p.after, p.expected_version)
                        .await?,
                )
            }
            ProposalList::NAME => api::result::<ProposalList>(queue_ops::proposals(self).await?),
            ProposalResolve::NAME => {
                let p = api::params::<ProposalResolve>(p)?;
                api::result::<ProposalResolve>(
                    queue_ops::resolve(self, p.proposal_id, p.accept).await?,
                )
            }
            WorkspaceList::NAME => api::result::<WorkspaceList>(self.workspace_views().await?),
            console_proto::api::TurnSubmit::NAME => {
                let p = api::params::<console_proto::api::TurnSubmit>(p)?;
                api::result::<console_proto::api::TurnSubmit>(
                    self.submit_turn(&p.text, &p.idempotency_key).await?,
                )
            }
            console_proto::api::TurnOverrideMethod::NAME => {
                let p = api::params::<console_proto::api::TurnOverrideMethod>(p)?;
                self.override_turn(p.turn_id, p.mode).await?;
                api::result::<console_proto::api::TurnOverrideMethod>(console_proto::api::Empty {})
            }
            console_proto::api::WorkerPrompt::NAME => {
                let p = api::params::<console_proto::api::WorkerPrompt>(p)?;
                self.message_worker(p.task_id, &p.text)
                    .await
                    .map_err(|e| api::ApiError(RpcError::rejected(e)))?;
                api::result::<console_proto::api::WorkerPrompt>(console_proto::api::Empty {})
            }
            console_proto::api::TaskRetry::NAME => {
                let p = api::params::<console_proto::api::TaskRetry>(p)?;
                let t = work::retry(self, p.task_id)
                    .await
                    .map_err(|e| api::ApiError(RpcError::rejected(e)))?;
                api::result::<console_proto::api::TaskRetry>(t.view())
            }
            console_proto::api::TaskResolve::NAME => {
                let p = api::params::<console_proto::api::TaskResolve>(p)?;
                let t = work::resolve(self, p.task_id, p.outcome)
                    .await
                    .map_err(|e| api::ApiError(RpcError::rejected(e)))?;
                api::result::<console_proto::api::TaskResolve>(t.view())
            }
            console_proto::api::Inspect::NAME => {
                let p = api::params::<console_proto::api::Inspect>(p)?;
                let text = inspect::inspect(self, &p.id)
                    .await
                    .map_err(|e| api::ApiError(RpcError::not_found(e)))?;
                api::result::<console_proto::api::Inspect>(console_proto::api::InspectResult {
                    text,
                })
            }
            console_proto::api::TaskPatch::NAME => {
                let p = api::params::<console_proto::api::TaskPatch>(p)?;
                let task = self.store.task(p.task_id).await?;
                let patch = self.store.latest_artifact(p.task_id, "patch").await?;
                api::result::<console_proto::api::TaskPatch>(console_proto::api::TaskPatchResult {
                    patch: patch.map(|a| a.content),
                    summary: task.and_then(|t| t.summary),
                })
            }
            HOOK_PROMPT_SUBMIT => Ok(console_agent::prompt_submit_output(
                &work::snapshot(self).await,
            )),
            HOOK_PRE_TOOL => {
                let input: console_agent::HookInput = serde_json::from_value(p).unwrap_or_default();
                Ok(
                    match work::pre_tool(self, input.tool_name.as_deref().unwrap_or("")).await {
                        Some(reason) => console_agent::pre_tool_deny(&reason),
                        None => Value::Object(serde_json::Map::new()),
                    },
                )
            }
            other => Err(api::ApiError(RpcError::new(
                RpcError::METHOD_NOT_FOUND,
                format!("unknown method {other}"),
            ))),
        }
    }

    pub(crate) async fn workspace_views(
        &self,
    ) -> api::ApiResult<Vec<console_proto::api::WorkspaceView>> {
        let executors = self.store.executors().await?;
        Ok(self
            .store
            .workspaces()
            .await?
            .into_iter()
            .map(|w| {
                let exec = executors.iter().find(|e| e.id == w.executor_id);
                console_proto::api::WorkspaceView {
                    id: w.id,
                    name: w.name,
                    executor_id: w.executor_id,
                    executor_name: exec.map(|e| e.name.clone()).unwrap_or_default(),
                    online: exec.is_some_and(|e| e.online),
                    head_rev: w.head_rev,
                    dirty: w.dirty,
                }
            })
            .collect())
    }

    /// The top of the queue, for attach snapshots.
    pub(crate) async fn queue_top(&self) -> api::ApiResult<Vec<TodoView>> {
        let mut q = queue_ops::list(self, false).await?;
        q.truncate(QUEUE_TOP);
        Ok(q)
    }

    /// Records a user message as a turn and queues it for the agent.
    pub async fn submit_turn(
        &self,
        text: &str,
        key: &str,
    ) -> api::ApiResult<console_proto::api::TurnSubmitResult> {
        if text.trim().is_empty() {
            return Err(api::ApiError(RpcError::invalid_params("empty message")));
        }
        let sid = self.orchestrator_session;
        let (turn, created) = self
            .store
            .write(async |tx| tx.insert_turn(sid, key, text).await)
            .await?;
        if created {
            self.agent.enqueue(turn.id);
        }
        Ok(console_proto::api::TurnSubmitResult {
            turn_id: turn.id,
            duplicate: !created,
        })
    }

    /// `/delegate` or `/inline` on a turn: recorded, then acted on with a follow-up turn.
    pub async fn override_turn(
        &self,
        turn: console_proto::TurnId,
        mode: console_proto::domain::TurnOverride,
    ) -> api::ApiResult<()> {
        self.store
            .write(async |tx| tx.set_turn_override(turn, mode).await)
            .await?;
        let text = match mode {
            console_proto::domain::TurnOverride::Delegate => {
                "/delegate — The user wants the previous request handed to a worker. Call delegate_task now with a complete brief."
            }
            console_proto::domain::TurnOverride::Inline => {
                "/inline — The user wants the previous request answered here, without delegating. Answer within your inline tools."
            }
        };
        self.submit_turn(text, &format!("override:{turn}:{mode:?}"))
            .await
            .map(|_| ())
    }

    /// Steers a running worker.
    pub async fn message_worker(
        &self,
        task: console_proto::TaskId,
        text: &str,
    ) -> Result<String, String> {
        worker_link::message_worker(self, task, text).await
    }

    /// Resolves a decision with an option or a free-text answer and routes it.
    pub async fn answer_decision(
        &self,
        id: console_proto::DecisionId,
        option_id: Option<String>,
        answer: Option<String>,
    ) -> Result<String, String> {
        let d = self
            .store
            .decision(id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("no such decision")?;
        let option = match (&option_id, d.options.is_empty()) {
            (Some(o), false) if d.options.iter().any(|x| &x.id == o) => o.clone(),
            (_, false) => {
                let ids: Vec<&str> = d.options.iter().map(|o| o.id.as_str()).collect();
                return Err(format!("choose one of: {}", ids.join(", ")));
            }
            (_, true) => "answer".to_owned(),
        };
        let ans = answer.clone();
        let record = self
            .store
            .write(async |tx| tx.resolve_decision(id, &option, ans.as_deref()).await)
            .await
            .map_err(|e| e.to_string())?;
        self.on_decision_resolved(&record, answer.as_deref()).await;
        Ok(format!("Answered: {}", answer.unwrap_or(option)))
    }

    /// A cancel was recorded; tell the executor running the task.
    pub(crate) async fn on_cancel_requested(&self, task: &TaskRecord) {
        if let Err(e) = self.hub.cancel(self, task.id).await {
            tracing::warn!(task = %task.id, error = %e, "could not send cancel");
        }
    }

    /// Worker traffic from an executor (progress, decisions, MCP calls).
    pub(crate) async fn on_worker_message(&self, executor: ExecutorId, msg: ExecutorMsg) {
        worker_link::on_message(self, executor, msg).await;
    }

    /// A command reached a final status.
    pub(crate) async fn after_command_settled(&self, _cmd: &CommandRecord, status: CommandStatus) {
        if matches!(
            status,
            CommandStatus::Succeeded
                | CommandStatus::Failed
                | CommandStatus::Cancelled
                | CommandStatus::Interrupted
        ) {
            work::start_held(self).await;
        }
    }

    /// A decision was answered; route the answer to the worker.
    pub(crate) async fn on_decision_resolved(
        &self,
        decision: &DecisionRecord,
        answer: Option<&str>,
    ) {
        worker_link::route_answer(self, decision, answer).await;
    }
}

/// The store file inside the data directory.
#[must_use]
pub fn db_path(config: &Config) -> PathBuf {
    config.data_dir.join("orchestrator.db")
}

/// Runs the service until interrupted.
pub async fn run(config: Config) -> anyhow::Result<()> {
    let token = config.token()?;
    let store = Store::open(&db_path(&config)).await?;
    let listener = TcpListener::bind(config.orchestrator.listen).await?;
    tracing::info!(addr = %listener.local_addr()?, db = %db_path(&config).display(), "orchestrator listening");
    let app = App::new(config, store, token).await?;
    agent_runner::start(&app);
    tokio::select! {
        r = server::serve(app, listener) => r?,
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
    }
    Ok(())
}
