//! Rust client for the Orchestrator's client API, shared by the bridge and the CLI (M2).
//!
//! See `docs/specs/clients.md`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use console_proto::api::{EVENT_NOTIFICATION, Method};
use console_proto::rpc::{EventNotification, Incoming, Request, RpcError};
use futures::{SinkExt as _, StreamExt as _};
use serde_json::Value;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use tokio_tungstenite::tungstenite::http::HeaderValue;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("cannot connect to the Orchestrator at {url}: {source}")]
    Connect {
        url: String,
        #[source]
        source: Box<tokio_tungstenite::tungstenite::Error>,
    },
    #[error("the connection to the Orchestrator closed")]
    Closed,
    #[error(transparent)]
    Rpc(#[from] RpcError),
    #[error("bad message: {0}")]
    Protocol(String),
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>>;

/// A connected client. Cheap to clone; all clones share one connection.
#[derive(Debug, Clone)]
pub struct Client {
    out: mpsc::UnboundedSender<String>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
}

impl Client {
    /// Connects with the bearer token. Returns the client and a stream of pushed events.
    pub async fn connect(
        url: &str,
        token: &str,
    ) -> Result<(Self, mpsc::UnboundedReceiver<EventNotification>), ClientError> {
        let connect_err = |e| ClientError::Connect {
            url: url.to_owned(),
            source: Box::new(e),
        };
        let mut request = url.into_client_request().map_err(connect_err)?;
        let value = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|e| ClientError::Protocol(e.to_string()))?;
        request.headers_mut().insert("Authorization", value);
        let (ws, _) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(connect_err)?;
        let (mut sink, mut stream) = ws.split();

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let pending: Pending = Arc::default();

        tokio::spawn(async move {
            while let Some(msg) = out_rx.recv().await {
                if sink.send(Message::Text(msg.into())).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        let pending2 = pending.clone();
        tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                let text = match msg {
                    Message::Text(t) => t.to_string(),
                    Message::Close(_) => break,
                    _ => continue,
                };
                match serde_json::from_str::<Incoming>(&text) {
                    Ok(Incoming::Response(resp)) => {
                        let Some(id) = resp.id.as_u64() else { continue };
                        if let Some(tx) = pending2.lock().await.remove(&id) {
                            let outcome = match resp.error {
                                Some(e) => Err(e),
                                None => Ok(resp.result.unwrap_or(Value::Null)),
                            };
                            let _ = tx.send(outcome);
                        }
                    }
                    Ok(Incoming::Notification(n)) if n.method == EVENT_NOTIFICATION => {
                        if let Ok(note) = serde_json::from_value::<EventNotification>(n.params) {
                            let _ = events_tx.send(note);
                        }
                    }
                    Ok(Incoming::Notification(_)) => {}
                    Err(e) => tracing::warn!(error = %e, "unparseable message from Orchestrator"),
                }
            }
            // Fail everything still waiting.
            pending2.lock().await.clear();
        });

        Ok((
            Self {
                out: out_tx,
                pending,
                next_id: Arc::new(AtomicU64::new(1)),
            },
            events_rx,
        ))
    }

    /// Calls a typed method.
    pub async fn call<M: Method>(&self, params: M::Params) -> Result<M::Result, ClientError> {
        let params =
            serde_json::to_value(params).map_err(|e| ClientError::Protocol(e.to_string()))?;
        let value = self.call_raw(M::NAME, params).await?;
        serde_json::from_value(value)
            .map_err(|e| ClientError::Protocol(format!("{}: {e}", M::NAME)))
    }

    /// Calls a method by name with raw JSON params.
    pub async fn call_raw(&self, method: &str, params: Value) -> Result<Value, ClientError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let req = serde_json::to_string(&Request::new(id, method, params))
            .map_err(|e| ClientError::Protocol(e.to_string()))?;
        self.out.send(req).map_err(|_| ClientError::Closed)?;
        rx.await
            .map_err(|_| ClientError::Closed)?
            .map_err(ClientError::Rpc)
    }
}
