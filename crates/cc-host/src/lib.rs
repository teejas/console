//! Hosts the Orchestrator's Claude Code CLI over stream-json, with one prompt in flight at a
//! time (M5).
//!
//! See `docs/specs/orchestrator-agent.md`. The caller sends one user message and reads
//! events until [`CcEvent::Result`] before sending the next, because Claude Code merges
//! queued prompts into one turn.

use std::path::PathBuf;
use std::process::Stdio;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::mpsc;

/// How to start the agent process.
#[derive(Debug, Clone)]
pub struct CcConfig {
    pub bin: PathBuf,
    /// Arguments after the stream-json flags (model, effort, settings, MCP config, prompt).
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    /// A Claude Code session to resume.
    pub resume: Option<String>,
}

/// What the agent did, translated from stream-json.
#[derive(Debug, Clone, PartialEq)]
pub enum CcEvent {
    /// The process is up; this is its session ID.
    Init { session_id: String },
    /// Streamed assistant text.
    TextDelta(String),
    /// The agent called a tool.
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    /// A tool returned.
    ToolResult {
        id: String,
        is_error: bool,
        text: String,
    },
    /// The turn finished.
    Result {
        session_id: Option<String>,
        is_error: bool,
        cost_usd: Option<f64>,
        text: Option<String>,
    },
    /// The process exited; `stderr` holds its last output.
    Exited { code: Option<i32>, stderr: String },
}

#[derive(Debug)]
pub struct CcProcess {
    stdin: ChildStdin,
    events: mpsc::UnboundedReceiver<CcEvent>,
    child: Child,
}

/// The fixed flags for programmatic, streamed turns.
pub const STREAM_FLAGS: &[&str] = &[
    "-p",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    "--verbose",
    "--include-partial-messages",
];

impl CcProcess {
    pub async fn spawn(cfg: &CcConfig) -> std::io::Result<Self> {
        std::fs::create_dir_all(&cfg.cwd)?;
        let mut cmd = tokio::process::Command::new(&cfg.bin);
        cmd.args(STREAM_FLAGS)
            .args(&cfg.args)
            .current_dir(&cfg.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(r) = &cfg.resume {
            cmd.args(["--resume", r]);
        }
        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("no stdout"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| std::io::Error::other("no stderr"))?;
        let (tx, events) = mpsc::unbounded_channel();

        let err_tx = tx.clone();
        let stderr_buf = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let sb = stderr_buf.clone();
        let stderr_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf).await;
            let text = String::from_utf8_lossy(&buf);
            let tail: String = text
                .chars()
                .rev()
                .take(2000)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            *sb.lock().await = tail;
        });
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    tracing::debug!(line = %line, "non-JSON output from Claude Code");
                    continue;
                };
                for event in translate(&v) {
                    if tx.send(event).is_err() {
                        return;
                    }
                }
            }
            let _ = stderr_task.await;
            let stderr = stderr_buf.lock().await.clone();
            let _ = err_tx.send(CcEvent::Exited { code: None, stderr });
        });
        Ok(Self {
            stdin,
            events,
            child,
        })
    }

    /// Sends one user message. Wait for [`CcEvent::Result`] before sending another.
    pub async fn send_user(&mut self, text: &str) -> std::io::Result<()> {
        let msg = json!({ "type": "user", "message": { "role": "user", "content": text } });
        let mut line = serde_json::to_vec(&msg)?;
        line.push(b'\n');
        self.stdin.write_all(&line).await?;
        self.stdin.flush().await
    }

    pub async fn next(&mut self) -> Option<CcEvent> {
        let ev = self.events.recv().await?;
        if let CcEvent::Exited { stderr, .. } = ev {
            let code = self.child.wait().await.ok().and_then(|s| s.code());
            return Some(CcEvent::Exited { code, stderr });
        }
        Some(ev)
    }

    pub async fn kill(&mut self) {
        let _ = self.child.kill().await;
    }
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|i| i.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Translates one stream-json line into events. Assistant text arrives through partial
/// `stream_event` deltas; the full `assistant` message is used only for tool calls.
#[must_use]
pub fn translate(v: &Value) -> Vec<CcEvent> {
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    match v.get("type").and_then(Value::as_str) {
        Some("system") if v.get("subtype").and_then(Value::as_str) == Some("init") => {
            s(v, "session_id")
                .map(|session_id| CcEvent::Init { session_id })
                .into_iter()
                .collect()
        }
        Some("stream_event") => {
            let ev = &v["event"];
            if ev["type"] == "content_block_delta" && ev["delta"]["type"] == "text_delta" {
                s(&ev["delta"], "text")
                    .map(CcEvent::TextDelta)
                    .into_iter()
                    .collect()
            } else {
                Vec::new()
            }
        }
        Some("assistant") => v["message"]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| b["type"] == "tool_use")
            .map(|b| CcEvent::ToolUse {
                id: s(b, "id").unwrap_or_default(),
                name: s(b, "name").unwrap_or_default(),
                input: b["input"].clone(),
            })
            .collect(),
        Some("user") => v["message"]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| b["type"] == "tool_result")
            .map(|b| CcEvent::ToolResult {
                id: s(b, "tool_use_id").unwrap_or_default(),
                is_error: b["is_error"].as_bool().unwrap_or(false),
                text: text_of(&b["content"]),
            })
            .collect(),
        Some("result") => vec![CcEvent::Result {
            session_id: s(v, "session_id"),
            is_error: v["is_error"].as_bool().unwrap_or(false),
            cost_usd: v["total_cost_usd"].as_f64(),
            text: s(v, "result"),
        }],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_the_stream_json_shapes_seen_in_m0() {
        let init = json!({"type":"system","subtype":"init","session_id":"s1","tools":[]});
        assert_eq!(
            translate(&init),
            vec![CcEvent::Init {
                session_id: "s1".into()
            }]
        );
        let delta = json!({"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}});
        assert_eq!(translate(&delta), vec![CcEvent::TextDelta("Hel".into())]);
        let assistant = json!({"type":"assistant","message":{"content":[
            {"type":"text","text":"ignored, already streamed"},
            {"type":"tool_use","id":"tu1","name":"mcp__console__queue_list","input":{}}]}});
        assert_eq!(
            translate(&assistant),
            vec![CcEvent::ToolUse {
                id: "tu1".into(),
                name: "mcp__console__queue_list".into(),
                input: json!({})
            }]
        );
        let result_msg = json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"tu1","content":[{"type":"text","text":"3 items"}]}]}});
        assert_eq!(
            translate(&result_msg),
            vec![CcEvent::ToolResult {
                id: "tu1".into(),
                is_error: false,
                text: "3 items".into()
            }]
        );
        let done = json!({"type":"result","subtype":"success","is_error":false,"session_id":"s1","total_cost_usd":0.01,"result":"Hi"});
        assert_eq!(
            translate(&done),
            vec![CcEvent::Result {
                session_id: Some("s1".into()),
                is_error: false,
                cost_usd: Some(0.01),
                text: Some("Hi".into())
            }]
        );
        assert!(translate(&json!({"type":"rate_limit_event"})).is_empty());
    }
}
