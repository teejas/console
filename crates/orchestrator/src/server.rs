//! HTTP and WebSocket endpoints: `/client` (JSON-RPC client API) and `/healthz`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use console_proto::rpc::{Request, Response as RpcResponse, RpcError};
use futures::{SinkExt as _, StreamExt as _};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use crate::App;

/// Builds the router.
pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/client", get(client_ws))
        .route("/executor", get(crate::hub::executor_ws))
        .with_state(app)
}

pub async fn serve(app: Arc<App>, listener: TcpListener) -> std::io::Result<()> {
    axum::serve(listener, router(app)).await
}

/// Checks the bearer token from the `Authorization` header or a `token` query parameter.
pub fn authorized(app: &App, headers: &HeaderMap, query: &HashMap<String, String>) -> bool {
    let header = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let given = header.or_else(|| query.get("token").map(String::as_str));
    given.is_some_and(|t| constant_time_eq(t.as_bytes(), app.token.as_bytes()))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0_u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn client_ws(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Response {
    if !authorized(&app, &headers, &query) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    ws.on_upgrade(move |socket| client_conn(app, socket))
}

/// One client connection: requests are handled concurrently; responses and event
/// notifications share one outbound queue.
#[derive(Debug, Clone)]
pub struct Conn {
    out: mpsc::UnboundedSender<String>,
    subs: Arc<std::sync::Mutex<Vec<AbortHandle>>>,
    next_sub: Arc<std::sync::atomic::AtomicU64>,
}

impl Conn {
    /// Queues a raw JSON message to the client. Returns false once the client is gone.
    pub fn send(&self, msg: String) -> bool {
        self.out.send(msg).is_ok()
    }

    pub fn track(&self, handle: AbortHandle) {
        if let Ok(mut subs) = self.subs.lock() {
            subs.push(handle);
        }
    }

    pub fn next_subscription_id(&self) -> String {
        let n = self
            .next_sub
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("sub-{n}")
    }

    pub fn is_closed(&self) -> bool {
        self.out.is_closed()
    }
}

async fn client_conn(app: Arc<App>, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if sink.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
        }
    });
    let conn = Conn {
        out: out_tx,
        subs: Arc::default(),
        next_sub: Arc::default(),
    };
    while let Some(Ok(msg)) = stream.next().await {
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Close(_) => break,
            _ => continue,
        };
        let app = app.clone();
        let conn2 = conn.clone();
        tokio::spawn(async move {
            if let Some(reply) = handle_text(&app, &conn2, &text).await {
                conn2.send(reply);
            }
        });
    }
    if let Ok(subs) = conn.subs.lock() {
        for s in subs.iter() {
            s.abort();
        }
    }
    drop(conn);
    writer.abort();
}

async fn handle_text(app: &Arc<App>, conn: &Conn, text: &str) -> Option<String> {
    let req: Request = match serde_json::from_str(text) {
        Ok(r) => r,
        Err(e) => {
            let resp = RpcResponse::err(
                Value::Null,
                RpcError::new(RpcError::PARSE_ERROR, e.to_string()),
            );
            return serde_json::to_string(&resp).ok();
        }
    };
    let id = req.id.clone()?; // Notifications get no reply.
    let resp = match crate::api::dispatch(app, conn, &req.method, req.params).await {
        Ok(result) => RpcResponse::ok(id, result),
        Err(error) => {
            tracing::debug!(method = %req.method, %error, "request failed");
            RpcResponse::err(id, error)
        }
    };
    serde_json::to_string(&resp).ok()
}
