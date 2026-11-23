//! The executor's workspace registry and command helpers shared by read ops and `Exec`.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use console_config::WorkspaceConfig;
use console_proto::WorkspaceId;
use console_proto::link::WorkspaceInfo;
use tokio::io::AsyncReadExt as _;

use crate::journal::Journal;

/// Largest output any read op or command returns.
pub const OUTPUT_CAP: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    /// Canonical absolute path.
    pub root: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct Registry {
    by_id: HashMap<WorkspaceId, Workspace>,
}

impl Registry {
    /// Registers configured workspaces, skipping (and logging) ones that don't exist.
    pub async fn load(configs: &[WorkspaceConfig], journal: &Journal) -> anyhow::Result<Self> {
        let mut by_id = HashMap::new();
        for c in configs {
            let root = match std::fs::canonicalize(&c.path) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(workspace = %c.name, path = %c.path.display(), error = %e, "skipping workspace");
                    continue;
                }
            };
            let id = journal.workspace_id(&root.to_string_lossy()).await?;
            by_id.insert(
                id,
                Workspace {
                    id,
                    name: c.name.clone(),
                    root,
                },
            );
        }
        Ok(Self { by_id })
    }

    pub fn get(&self, id: WorkspaceId) -> Option<&Workspace> {
        self.by_id.get(&id)
    }

    pub fn all(&self) -> impl Iterator<Item = &Workspace> {
        self.by_id.values()
    }

    /// Current workspace descriptions for `Hello`.
    pub async fn infos(&self) -> Vec<WorkspaceInfo> {
        let mut infos = Vec::new();
        for w in self.all() {
            let git = |args: &'static [&'static str]| {
                run_capture(&w.root, "git", args, Duration::from_secs(5))
            };
            let remote = git(&["remote", "get-url", "origin"])
                .await
                .ok()
                .filter(|o| o.ok)
                .map(|o| o.stdout.trim().to_owned());
            let head = git(&["rev-parse", "HEAD"])
                .await
                .ok()
                .filter(|o| o.ok)
                .map(|o| o.stdout.trim().to_owned());
            let dirty = git(&["status", "--porcelain"])
                .await
                .ok()
                .is_some_and(|o| o.ok && !o.stdout.trim().is_empty());
            infos.push(WorkspaceInfo {
                workspace_id: w.id,
                name: w.name.clone(),
                path: w.root.to_string_lossy().into_owned(),
                repo_remote: remote,
                head_rev: head,
                dirty,
            });
        }
        infos.sort_by(|a, b| a.name.cmp(&b.name));
        infos
    }
}

impl Workspace {
    /// Resolves a workspace-relative path, refusing anything that escapes the root.
    pub fn resolve(&self, rel: &str) -> Result<PathBuf, String> {
        let rel = Path::new(rel.trim_start_matches("./"));
        if rel.is_absolute() || rel.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(format!(
                "path {} must be relative to the workspace",
                rel.display()
            ));
        }
        let joined = self.root.join(rel);
        let canonical =
            std::fs::canonicalize(&joined).map_err(|e| format!("{}: {e}", rel.display()))?;
        if !canonical.starts_with(&self.root) {
            return Err(format!("{} is outside the workspace", rel.display()));
        }
        Ok(canonical)
    }
}

#[derive(Debug, Clone)]
pub struct Captured {
    pub ok: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

/// Runs a short command with a timeout and capped output. The child runs in its own
/// process group and is killed with it on timeout.
pub async fn run_capture(
    dir: &Path,
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> std::io::Result<Captured> {
    let argv: Vec<String> = std::iter::once(program.to_owned())
        .chain(args.iter().map(|s| (*s).to_owned()))
        .collect();
    run_argv(dir, &argv, timeout).await
}

pub async fn run_argv(dir: &Path, argv: &[String], timeout: Duration) -> std::io::Result<Captured> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| std::io::Error::other("empty command"))?;
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()?;
    let pid = child.id();
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| std::io::Error::other("no stdout"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| std::io::Error::other("no stderr"))?;
    let read_out = async {
        let mut buf = Vec::new();
        let _ = (&mut stdout)
            .take(OUTPUT_CAP as u64 + 1)
            .read_to_end(&mut buf)
            .await;
        // Drain the rest so the child never blocks on a full pipe.
        let _ = tokio::io::copy(&mut stdout, &mut tokio::io::sink()).await;
        buf
    };
    let read_err = async {
        let mut buf = Vec::new();
        let _ = (&mut stderr).take(16 * 1024).read_to_end(&mut buf).await;
        let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
        buf
    };
    let run = async { tokio::join!(read_out, read_err, child.wait()) };
    match tokio::time::timeout(timeout, run).await {
        Ok((out, err, status)) => {
            let status = status?;
            let truncated = out.len() > OUTPUT_CAP;
            let out = &out[..out.len().min(OUTPUT_CAP)];
            Ok(Captured {
                ok: status.success(),
                code: status.code(),
                stdout: String::from_utf8_lossy(out).into_owned(),
                stderr: String::from_utf8_lossy(&err).into_owned(),
                truncated,
            })
        }
        Err(_) => {
            if let Some(pid) = pid {
                kill_group(pid);
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("timed out after {}s", timeout.as_secs()),
            ))
        }
    }
}

/// Sends SIGKILL to a process group (the child was started as its group leader).
pub fn kill_group(pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
}

/// Sends SIGTERM to a process group, for a graceful stop.
pub fn term_group(pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGTERM,
        );
    }
}
