//! Operating commands: `up`, `down`, `status`, `inspect`, `logs`, `retry`, `resolve`.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, anyhow, bail};
use console_client::Client;
use console_config::Config;
use console_proto::TaskId;
use console_proto::api::{
    AttachParams, Empty, EventsSubscribe, Inspect, InspectParams, QueueList, QueueListParams,
    ResolvedOutcome, SessionAttach, SessionTarget, SubscribeParams, TaskGetParams, TaskList,
    TaskListParams, TaskResolve, TaskResolveParams, TaskRetry, WorkspaceList,
};
use console_proto::event::EventBody;

use crate::queue_cmd::connect;

/// The two background services.
const SERVICES: [(&str, &str); 2] = [
    ("com.console.orchestrator", "orchestrator"),
    ("com.console.executor", "executor"),
];

fn uid() -> anyhow::Result<String> {
    let out = std::process::Command::new("id").arg("-u").output()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// A launchd agent definition for one service.
#[must_use]
pub fn plist(
    label: &str,
    exe: &Path,
    config: Option<&Path>,
    role: &str,
    log: &Path,
    path_env: &str,
) -> String {
    let mut args = vec![exe.display().to_string()];
    if let Some(c) = config {
        args.push("--config".into());
        args.push(c.display().to_string());
    }
    args.push(role.into());
    let args: String = args
        .iter()
        .map(|a| format!("    <string>{}</string>\n", xml(a)))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{label}</string>
  <key>ProgramArguments</key>
  <array>
{args}  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>{path}</string>
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ThrottleInterval</key>
  <integer>5</integer>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
        label = xml(label),
        path = xml(path_env),
        log = xml(&log.display().to_string()),
    )
}

fn agents_dir(over: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    match over {
        Some(d) => Ok(d),
        None => Ok(directories::BaseDirs::new()
            .ok_or_else(|| anyhow!("no home directory"))?
            .home_dir()
            .join("Library/LaunchAgents")),
    }
}

fn label(prefix: Option<&str>, base: &str) -> String {
    prefix.map_or_else(
        || base.to_owned(),
        |p| format!("{p}.{}", base.trim_start_matches("com.console.")),
    )
}

/// `console up`: install and start both services as launchd agents.
pub fn up(
    config: &Config,
    config_path: Option<&Path>,
    dir: Option<PathBuf>,
    prefix: Option<&str>,
) -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let dir = agents_dir(dir)?;
    std::fs::create_dir_all(&dir)?;
    let logs = config.data_dir.join("logs");
    std::fs::create_dir_all(&logs)?;
    let path_env = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin:/usr/sbin:/sbin".into());
    let domain = format!("gui/{}", uid()?);
    for (base, role) in SERVICES {
        let label = label(prefix, base);
        let file = dir.join(format!("{label}.plist"));
        std::fs::write(
            &file,
            plist(
                &label,
                &exe,
                config_path,
                role,
                &logs.join(format!("{role}.log")),
                &path_env,
            ),
        )?;
        let _ = std::process::Command::new("launchctl")
            .args(["bootout", &format!("{domain}/{label}")])
            .output();
        let out = std::process::Command::new("launchctl")
            .args(["bootstrap", &domain])
            .arg(&file)
            .output()?;
        if !out.status.success() {
            bail!(
                "launchctl bootstrap {label}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        println!("Started {label} ({})", file.display());
    }
    println!("Logs: {}", logs.display());
    Ok(())
}

/// `console down`: stop both services and remove their agents so they don't start at login.
pub fn down(dir: Option<PathBuf>, prefix: Option<&str>) -> anyhow::Result<()> {
    let dir = agents_dir(dir)?;
    let domain = format!("gui/{}", uid()?);
    for (base, _) in SERVICES {
        let label = label(prefix, base);
        let out = std::process::Command::new("launchctl")
            .args(["bootout", &format!("{domain}/{label}")])
            .output()?;
        let file = dir.join(format!("{label}.plist"));
        let _ = std::fs::remove_file(&file);
        println!(
            "{} {label}",
            if out.status.success() {
                "Stopped"
            } else {
                "Not running:"
            }
        );
    }
    Ok(())
}

fn service_state(domain: &str, label: &str) -> String {
    let Ok(out) = std::process::Command::new("launchctl")
        .args(["print", &format!("{domain}/{label}")])
        .output()
    else {
        return "unknown".into();
    };
    if !out.status.success() {
        return "not installed".into();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let field = |k: &str| {
        text.lines()
            .find_map(|l| l.trim().strip_prefix(&format!("{k} = ")).map(str::to_owned))
    };
    match (field("state"), field("pid")) {
        (Some(s), Some(p)) => format!("{s} (pid {p})"),
        (Some(s), None) => s,
        _ => "loaded".into(),
    }
}

/// `console status`: services, the Orchestrator, executors, and what's in flight.
pub async fn status(config: &Config, prefix: Option<&str>) -> anyhow::Result<()> {
    if let Ok(uid) = uid() {
        let domain = format!("gui/{uid}");
        for (base, role) in SERVICES {
            println!(
                "{role:<13} {}",
                service_state(&domain, &label(prefix, base))
            );
        }
    }
    let client = match connect(config).await {
        Ok(c) => c,
        Err(_) => {
            println!("Orchestrator  unreachable at {}", config.client_url());
            return Ok(());
        }
    };
    println!("Orchestrator  reachable at {}", config.client_url());
    let ws = client.call::<WorkspaceList>(Empty {}).await?;
    if ws.is_empty() {
        println!(
            "Workspaces    none registered (is the executor running with workspaces configured?)"
        );
    }
    for w in ws {
        println!(
            "Workspace     {} on {} ({}{})",
            w.name,
            w.executor_name,
            if w.online { "online" } else { "offline" },
            if w.dirty { ", uncommitted changes" } else { "" }
        );
    }
    let tasks = client.call::<TaskList>(TaskListParams::default()).await?;
    let queue = client.call::<QueueList>(QueueListParams::default()).await?;
    let decisions = client
        .call::<SessionAttach>(AttachParams {
            target: SessionTarget::Orchestrator,
            recent: 0,
        })
        .await?
        .open_decisions;
    println!("Active tasks  {}", tasks.len());
    println!("Queue         {} open", queue.len());
    println!("Waiting on you {}", decisions.len());
    Ok(())
}

/// Resolves a task by its number in `console tasks` or a unique ID suffix.
async fn find_task(client: &Client, reference: &str) -> anyhow::Result<TaskId> {
    let all = client
        .call::<TaskList>(TaskListParams { include_done: true })
        .await?;
    let active: Vec<_> = all.iter().filter(|t| !t.state.is_terminal()).collect();
    if let Ok(n) = reference.parse::<usize>()
        && (1..=active.len()).contains(&n)
    {
        return Ok(active[n - 1].id);
    }
    let needle = reference.to_lowercase();
    let m: Vec<_> = all
        .iter()
        .filter(|t| t.id.to_string().to_lowercase().ends_with(&needle))
        .collect();
    match m.as_slice() {
        [t] => Ok(t.id),
        [] => bail!("no task matches {reference:?}; see `console tasks --all`"),
        _ => bail!("{reference:?} matches several tasks; use more of the ID"),
    }
}

pub async fn inspect(config: &Config, id: &str) -> anyhow::Result<()> {
    let client = connect(config).await?;
    let r = client
        .call::<Inspect>(InspectParams { id: id.to_owned() })
        .await?;
    print!("{}", r.text);
    Ok(())
}

pub async fn retry(config: &Config, reference: &str) -> anyhow::Result<()> {
    let client = connect(config).await?;
    let id = find_task(&client, reference).await?;
    let t = client
        .call::<TaskRetry>(TaskGetParams { task_id: id })
        .await?;
    println!("Retrying \"{}\" ({:?}).", t.title, t.state);
    Ok(())
}

pub async fn resolve(config: &Config, reference: &str, outcome: &str) -> anyhow::Result<()> {
    let client = connect(config).await?;
    let id = find_task(&client, reference).await?;
    let outcome: ResolvedOutcome =
        serde_json::from_value(serde_json::Value::String(outcome.to_lowercase()))
            .map_err(|_| anyhow!("outcome must be succeeded, failed, or cancelled"))?;
    let t = client
        .call::<TaskResolve>(TaskResolveParams {
            task_id: id,
            outcome,
        })
        .await?;
    println!("\"{}\" is now {:?}.", t.title, t.state);
    Ok(())
}

/// `console logs`: a task's worker session, optionally following it live.
pub async fn logs(config: &Config, reference: &str, follow: bool) -> anyhow::Result<()> {
    let token = config.token().context("reading the local token")?;
    let (client, mut events) = Client::connect(&config.client_url(), &token)
        .await
        .context("is the Orchestrator running?")?;
    let id = find_task(&client, reference).await?;
    let a = client
        .call::<SessionAttach>(AttachParams {
            target: SessionTarget::Task { task_id: id },
            recent: 2000,
        })
        .await?;
    let print = |body: &EventBody| match body {
        EventBody::AgentText { text, .. } => print!("{text}"),
        EventBody::UserMessage { text, .. } => println!("\n> {text}"),
        EventBody::ToolCallStarted { title, .. } => println!("\n  ↳ {title}"),
        EventBody::ToolCallFinished { ok: false, .. } => println!("    (failed)"),
        EventBody::TaskTransition { to, blocked, .. } => {
            println!(
                "\n[{:?}{}]",
                to,
                blocked.map(|b| format!(", {b:?}")).unwrap_or_default()
            );
        }
        EventBody::DecisionRequested { question, .. } => println!("\n[needs you] {question}"),
        _ => {}
    };
    for e in &a.recent_events {
        print(&e.body);
    }
    if !follow {
        println!();
        return Ok(());
    }
    client
        .call::<EventsSubscribe>(SubscribeParams {
            session_id: a.session_id,
            after: a.cursor,
        })
        .await?;
    while let Some(n) = events.recv().await {
        print(&n.event.body);
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
        if let EventBody::TaskTransition { to, .. } = n.event.body
            && to.is_terminal()
        {
            println!();
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_runs_the_role_with_config_path_and_logs() {
        let p = plist(
            "com.console.executor",
            Path::new("/opt/console"),
            Some(Path::new("/home/me/.console/config.toml")),
            "executor",
            Path::new("/home/me/.console/logs/executor.log"),
            "/usr/bin:/opt/homebrew/bin",
        );
        assert!(p.contains("<string>/opt/console</string>\n    <string>--config</string>\n    <string>/home/me/.console/config.toml</string>\n    <string>executor</string>"));
        assert!(p.contains("<key>KeepAlive</key>\n  <true/>"));
        assert!(p.contains("/opt/homebrew/bin"));
        assert_eq!(
            label(Some("test.x"), "com.console.executor"),
            "test.x.executor"
        );
    }
}
