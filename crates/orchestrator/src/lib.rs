//! The Orchestrator service: client API server, task engine, dispatcher, executor hub (M2).
//!
//! See `docs/specs/topology.md`.

pub mod api;
pub mod engine;
pub mod hub;
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

    /// Methods added by later milestones.
    pub(crate) async fn dispatch_more(
        self: &Arc<Self>,
        _conn: &Conn,
        method: &str,
        _params: Value,
    ) -> api::ApiResult<Value> {
        Err(api::ApiError(RpcError::new(
            RpcError::METHOD_NOT_FOUND,
            format!("unknown method {method}"),
        )))
    }

    /// The top of the queue, for attach snapshots.
    pub(crate) async fn queue_top(&self) -> api::ApiResult<Vec<TodoView>> {
        Ok(Vec::new())
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
