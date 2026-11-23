//! The Orchestrator service: client API server, task engine, dispatcher, executor hub (M2).
//!
//! See `docs/specs/topology.md`.

pub mod api;
pub mod engine;
pub mod hub;
pub mod queue_ops;
pub mod server;
pub mod task_machine;

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

    /// A cancel was recorded; tell the executor running the task.
    pub(crate) async fn on_cancel_requested(&self, task: &TaskRecord) {
        if let Err(e) = self.hub.cancel(self, task.id).await {
            tracing::warn!(task = %task.id, error = %e, "could not send cancel");
        }
    }

    /// Worker traffic from an executor (progress, decisions, MCP calls).
    pub(crate) async fn on_worker_message(&self, _executor: ExecutorId, msg: ExecutorMsg) {
        tracing::debug!(?msg, "worker message ignored until workers exist");
    }

    /// A command reached a final status.
    pub(crate) async fn after_command_settled(&self, _cmd: &CommandRecord, _status: CommandStatus) {
    }

    /// A decision was answered; route the answer to the worker.
    pub(crate) async fn on_decision_resolved(
        &self,
        _decision: &DecisionRecord,
        _answer: Option<&str>,
    ) {
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
    tokio::select! {
        r = server::serve(app, listener) => r?,
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
    }
    Ok(())
}
