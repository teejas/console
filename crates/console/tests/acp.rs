//! `console acp` end to end: the real binary, driven over stdio by an ACP client (as Toad
//! does), against an in-process Orchestrator whose agent is a fake stream-json `claude`.

use std::str::FromStr as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::v1::schema::{
    ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest, SessionNotification,
    SessionUpdate, TextContent,
};
use agent_client_protocol::{AcpAgent, Agent};
use console_config::Config;
use console_orchestrator::{App, agent_runner, server};
use console_store::Store;

const FAKE_CLAUDE: &str = r#"#!/usr/bin/env python3
import sys, json
def out(o):
    print(json.dumps(o), flush=True)
out({"type": "system", "subtype": "init", "session_id": "s1"})
for line in sys.stdin:
    text = json.loads(line)["message"]["content"]
    out({"type": "stream_event", "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "Echo: " + text}}})
    out({"type": "result", "subtype": "success", "is_error": False, "session_id": "s1", "result": ""})
"#;

fn chunks(updates: &[SessionUpdate]) -> String {
    updates
        .iter()
        .filter_map(|u| match u {
            SessionUpdate::AgentMessageChunk(c) => match &c.content {
                ContentBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn toad_style_client_talks_to_the_orchestrator() {
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("claude");
    std::fs::write(&fake, FAKE_CLAUDE).unwrap();
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let data = dir.path().join("data");
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(
        &cfg_path,
        format!(
            "data_dir = \"{}\"\n[orchestrator]\nlisten = \"{addr}\"\n[agent]\nclaude_bin = \"{}\"\n",
            data.display(),
            fake.display()
        ),
    )
    .unwrap();
    let config = Config::load(Some(&cfg_path)).unwrap();
    let token = config.token().unwrap();
    let app = App::new(config, Store::open_in_memory().await.unwrap(), token)
        .await
        .unwrap();
    tokio::spawn(server::serve(app.clone(), listener));
    agent_runner::start(&app);

    let updates: Arc<Mutex<Vec<SessionUpdate>>> = Arc::default();
    let sink = updates.clone();
    let cmd = format!(
        "{} --config {} acp",
        env!("CARGO_BIN_EXE_console"),
        cfg_path.display()
    );
    let agent = AcpAgent::from_str(&cmd).unwrap();
    let seen = updates.clone();
    tokio::time::timeout(
        Duration::from_secs(30),
        agent_client_protocol::Client
            .v1()
            .on_receive_notification(
                async move |n: SessionNotification, _cx| {
                    sink.lock().unwrap().push(n.update);
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_with(agent, |cx: ConnectionTo<Agent>| async move {
                let init = cx.send_request(InitializeRequest::new(ProtocolVersion::V1)).block_task().await?;
                assert!(init.agent_capabilities.load_session);
                let session = cx.send_request(NewSessionRequest::new(dir.path().to_path_buf())).block_task().await?;
                let sid = session.session_id.clone();
                let say = |t: &str| PromptRequest::new(sid.clone(), vec![ContentBlock::Text(TextContent::new(t))]);

                // A normal message is an Orchestrator turn; the reply streams back.
                cx.send_request(say("hello there")).block_task().await?;
                assert!(chunks(&seen.lock().unwrap()).contains("Echo: hello there"));

                // Slash commands go through the client API.
                cx.send_request(say("/add write the docs")).block_task().await?;
                assert!(chunks(&seen.lock().unwrap()).contains("Added \"write the docs\""));
                cx.send_request(say("/queue")).block_task().await?;
                let has_plan = seen.lock().unwrap().iter().any(|u| matches!(u, SessionUpdate::Plan(p) if p.entries.iter().any(|e| e.content.contains("write the docs"))));
                assert!(has_plan, "the queue appears in the plan panel");
                cx.send_request(say("/bogus")).block_task().await?;
                assert!(chunks(&seen.lock().unwrap()).contains("Unknown command /bogus"));
                Ok(())
            }),
    )
    .await
    .expect("finished in time")
    .unwrap();

    let all = updates.lock().unwrap();
    assert!(
        all.iter().any(|u| matches!(u, SessionUpdate::AvailableCommandsUpdate(c) if c.available_commands.iter().any(|c| c.name == "queue"))),
        "slash commands are advertised"
    );
}
