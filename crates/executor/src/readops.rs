//! Bounded, read-only workspace lookups for the Orchestrator's inline tools.

use std::fmt::Write as _;
use std::time::Duration;

use console_proto::link::{ReadOp, ReadOpOutcome};
use grep_regex::RegexMatcher;
use grep_searcher::Searcher;
use grep_searcher::sinks::UTF8;

use crate::workspaces::{Captured, OUTPUT_CAP, Registry, run_argv, run_capture};

const MAX_DIR_ENTRIES: usize = 500;

/// Runs a read op within `deadline`.
pub async fn run(
    registry: &Registry,
    allowlist: &[Vec<String>],
    op: ReadOp,
    deadline: Duration,
) -> ReadOpOutcome {
    match tokio::time::timeout(deadline, run_inner(registry, allowlist, op, deadline)).await {
        Ok(outcome) => outcome,
        Err(_) => ReadOpOutcome::Failed {
            error: format!("timed out after {}ms", deadline.as_millis()),
        },
    }
}

fn workspace_id(op: &ReadOp) -> console_proto::WorkspaceId {
    match op {
        ReadOp::ListDir { workspace_id, .. }
        | ReadOp::ReadFile { workspace_id, .. }
        | ReadOp::Search { workspace_id, .. }
        | ReadOp::GitStatus { workspace_id }
        | ReadOp::GitLog { workspace_id, .. }
        | ReadOp::GitDiff { workspace_id, .. }
        | ReadOp::GitShow { workspace_id, .. }
        | ReadOp::RunReadonly { workspace_id, .. } => *workspace_id,
    }
}

fn ok(output: String, truncated: bool) -> ReadOpOutcome {
    ReadOpOutcome::Ok { output, truncated }
}

fn failed(error: impl Into<String>) -> ReadOpOutcome {
    ReadOpOutcome::Failed {
        error: error.into(),
    }
}

fn from_captured(c: std::io::Result<Captured>) -> ReadOpOutcome {
    match c {
        Ok(c) if c.ok => ok(c.stdout, c.truncated),
        Ok(c) => failed(format!(
            "exit {}: {}",
            c.code
                .map_or_else(|| "signal".to_owned(), |c| c.to_string()),
            c.stderr.trim()
        )),
        Err(e) => failed(e.to_string()),
    }
}

/// Rejects revision arguments that could be read as options.
fn safe_rev(rev: &str) -> Result<(), ReadOpOutcome> {
    if rev.starts_with('-') || rev.contains(char::is_whitespace) {
        Err(ReadOpOutcome::Denied {
            reason: format!("invalid revision {rev:?}"),
        })
    } else {
        Ok(())
    }
}

async fn run_inner(
    registry: &Registry,
    allowlist: &[Vec<String>],
    op: ReadOp,
    deadline: Duration,
) -> ReadOpOutcome {
    let Some(ws) = registry.get(workspace_id(&op)) else {
        return failed("unknown workspace on this executor");
    };
    match op {
        ReadOp::ListDir { path, .. } => {
            let dir = match ws.resolve(if path.is_empty() { "." } else { &path }) {
                Ok(d) => d,
                Err(e) => return ReadOpOutcome::Denied { reason: e },
            };
            let mut entries = match std::fs::read_dir(&dir) {
                Ok(rd) => rd
                    .filter_map(Result::ok)
                    .map(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        if e.file_type().is_ok_and(|t| t.is_dir()) {
                            format!("{name}/")
                        } else {
                            name
                        }
                    })
                    .collect::<Vec<_>>(),
                Err(e) => return failed(e.to_string()),
            };
            entries.sort();
            let truncated = entries.len() > MAX_DIR_ENTRIES;
            entries.truncate(MAX_DIR_ENTRIES);
            ok(entries.join("\n"), truncated)
        }
        ReadOp::ReadFile {
            path, max_bytes, ..
        } => {
            let file = match ws.resolve(&path) {
                Ok(f) => f,
                Err(e) => return ReadOpOutcome::Denied { reason: e },
            };
            let cap = usize::try_from(max_bytes)
                .unwrap_or(OUTPUT_CAP)
                .min(OUTPUT_CAP * 4);
            match tokio::fs::read(&file).await {
                Ok(bytes) => {
                    let truncated = bytes.len() > cap;
                    ok(
                        String::from_utf8_lossy(&bytes[..bytes.len().min(cap)]).into_owned(),
                        truncated,
                    )
                }
                Err(e) => failed(e.to_string()),
            }
        }
        ReadOp::Search {
            pattern,
            path_glob,
            max_matches,
            ..
        } => {
            let root = ws.root.clone();
            tokio::task::spawn_blocking(move || {
                search(&root, &pattern, path_glob.as_deref(), max_matches)
            })
            .await
            .unwrap_or_else(|e| failed(e.to_string()))
        }
        ReadOp::GitStatus { .. } => from_captured(
            run_capture(
                &ws.root,
                "git",
                &["status", "--short", "--branch"],
                deadline,
            )
            .await,
        ),
        ReadOp::GitLog { max, .. } => {
            let n = format!("-{}", max.clamp(1, 200));
            from_captured(
                run_capture(
                    &ws.root,
                    "git",
                    &["log", "--oneline", "--decorate", &n],
                    deadline,
                )
                .await,
            )
        }
        ReadOp::GitDiff { rev_range, .. } => {
            let mut argv = vec!["git".to_owned(), "diff".to_owned()];
            if let Some(r) = rev_range {
                if let Err(denied) = safe_rev(&r) {
                    return denied;
                }
                argv.push(r);
            }
            from_captured(run_argv(&ws.root, &argv, deadline).await)
        }
        ReadOp::GitShow { rev, .. } => {
            if let Err(denied) = safe_rev(&rev) {
                return denied;
            }
            let argv = ["git", "show", "--stat", "--patch", &rev].map(str::to_owned);
            from_captured(run_argv(&ws.root, &argv, deadline).await)
        }
        ReadOp::RunReadonly { argv, .. } => {
            let allowed = allowlist.iter().any(|prefix| {
                !prefix.is_empty()
                    && argv.len() >= prefix.len()
                    && argv[..prefix.len()] == prefix[..]
            });
            if !allowed {
                return ReadOpOutcome::Denied {
                    reason: format!("`{}` is not on the read-only allowlist", argv.join(" ")),
                };
            }
            from_captured(run_argv(&ws.root, &argv, deadline).await)
        }
    }
}

fn search(
    root: &std::path::Path,
    pattern: &str,
    glob: Option<&str>,
    max_matches: u32,
) -> ReadOpOutcome {
    let matcher = match RegexMatcher::new_line_matcher(pattern) {
        Ok(m) => m,
        Err(e) => return failed(format!("bad pattern: {e}")),
    };
    let glob = match glob.map(globset::Glob::new).transpose() {
        Ok(g) => g.map(|g| g.compile_matcher()),
        Err(e) => return failed(format!("bad glob: {e}")),
    };
    let max = usize::try_from(max_matches.clamp(1, 1000)).unwrap_or(100);
    let mut out = String::new();
    let mut count = 0;
    let mut truncated = false;
    let mut searcher = Searcher::new();
    for entry in ignore::WalkBuilder::new(root)
        .build()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
        if let Some(g) = &glob
            && !g.is_match(rel)
        {
            continue;
        }
        let _ = searcher.search_path(
            &matcher,
            entry.path(),
            UTF8(|line_no, line| {
                if count >= max || out.len() > OUTPUT_CAP {
                    truncated = true;
                    return Ok(false);
                }
                count += 1;
                let _ = write!(out, "{}:{line_no}:{}", rel.display(), line);
                if !line.ends_with('\n') {
                    out.push('\n');
                }
                Ok(true)
            }),
        );
        if truncated {
            break;
        }
    }
    if count == 0 {
        out = "no matches".into();
    }
    ok(out, truncated)
}
