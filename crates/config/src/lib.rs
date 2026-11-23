//! Layered configuration for every console component.
//!
//! Sources, lowest to highest precedence:
//! 1. Built-in defaults ([`Config::default`]).
//! 2. A TOML file: `--config <path>`, else `$CONSOLE_CONFIG`, else `<data_dir>/config.toml`.
//!    A missing file is fine; defaults apply.
//! 3. Environment variables prefixed `CONSOLE_`, with `__` separating nested keys
//!    (for example `CONSOLE_AGENT__EFFORT=high`).

use std::{
    collections::BTreeMap,
    io::{self, Read as _},
    net::SocketAddr,
    path::{Path, PathBuf},
};

use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use serde::{Deserialize, Serialize};

/// Environment variable that points at an alternative config file.
pub const CONFIG_ENV: &str = "CONSOLE_CONFIG";

/// `CONSOLE_*` variables that are not config overrides (lowercased, without the prefix):
/// the config file path, and what the Orchestrator passes to its hook commands.
pub const RESERVED_ENV: &[&str] = &["config", "hook_url", "hook_token"];

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not determine the home directory; set `data_dir` explicitly")]
    NoHome,
    #[error(transparent)]
    Invalid(#[from] Box<figment::Error>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Where all durable state lives (store, artifacts, journal, logs).
    pub data_dir: PathBuf,
    pub orchestrator: OrchestratorConfig,
    pub executor: ExecutorConfig,
    pub agent: AgentConfig,
    pub log: LogConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OrchestratorConfig {
    /// Address for the client API and the executor link.
    pub listen: SocketAddr,
    pub max_workers_per_workspace: u32,
    pub max_workers_total: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExecutorConfig {
    /// The Orchestrator's executor-link endpoint. The executor always dials out.
    pub orchestrator_url: String,
    /// Human-readable executor name shown in the UI.
    pub name: String,
    /// Worker harnesses by name: the command that starts each ACP adapter (installed, no npx).
    pub harnesses: BTreeMap<String, Vec<String>>,
    /// The harness used when a delegation doesn't name one.
    pub default_harness: String,
    /// Workspaces this executor owns.
    pub workspaces: Vec<WorkspaceConfig>,
    /// Commands the Orchestrator's `run_readonly` tool may run, by argv prefix.
    pub readonly_allowlist: Vec<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    /// Short name the Orchestrator and you use, for example `console`.
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    /// The Claude Code CLI used for the Orchestrator.
    pub claude_bin: PathBuf,
    pub model: String,
    pub effort: Effort,
    /// Inline budget per turn (`docs/specs/orchestrator-agent.md`).
    pub inline_max_tool_calls: u32,
    pub inline_max_wall_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    /// The value Claude Code's `--effort` flag expects.
    #[must_use]
    pub const fn as_flag(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    /// `tracing` filter directive; `RUST_LOG` overrides it when set.
    pub filter: String,
    /// Emit JSON lines instead of human-readable logs (for the daemons).
    pub json: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir().unwrap_or_else(|| PathBuf::from(".console")),
            orchestrator: OrchestratorConfig::default(),
            executor: ExecutorConfig::default(),
            agent: AgentConfig::default(),
            log: LogConfig::default(),
        }
    }
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 7600)),
            max_workers_per_workspace: 1,
            max_workers_total: 2,
        }
    }
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            orchestrator_url: "ws://127.0.0.1:7600/executor".into(),
            name: "laptop".into(),
            harnesses: BTreeMap::from([
                ("claude".to_string(), vec!["claude-agent-acp".to_string()]),
                ("codex".to_string(), vec!["codex-acp".to_string()]),
            ]),
            default_harness: "claude".into(),
            workspaces: Vec::new(),
            readonly_allowlist: [
                &["gh", "pr", "view"][..],
                &["gh", "pr", "diff"],
                &["gh", "pr", "checks"],
                &["cargo", "metadata"],
            ]
            .iter()
            .map(|argv| argv.iter().map(|s| (*s).to_string()).collect())
            .collect(),
        }
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            claude_bin: PathBuf::from("claude"),
            model: "claude-opus-5-5".into(),
            effort: Effort::Medium,
            inline_max_tool_calls: 10,
            inline_max_wall_secs: 90,
        }
    }
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            filter: "info".into(),
            json: false,
        }
    }
}

/// `~/.console`, or `None` if the home directory is unknown.
#[must_use]
pub fn default_data_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|d| d.home_dir().join(".console"))
}

impl Config {
    /// Loads the layered configuration. `explicit` is the `--config` flag, if given.
    pub fn load(explicit: Option<&Path>) -> Result<Self, ConfigError> {
        let path = Self::file_path(explicit)?;
        Self::figment(&path)
            .extract()
            .map_err(|e| ConfigError::Invalid(Box::new(e)))
    }

    /// The config file that [`Config::load`] reads (it may not exist).
    pub fn file_path(explicit: Option<&Path>) -> Result<PathBuf, ConfigError> {
        if let Some(p) = explicit {
            return Ok(p.to_path_buf());
        }
        if let Some(p) = std::env::var_os(CONFIG_ENV) {
            return Ok(PathBuf::from(p));
        }
        default_data_dir()
            .map(|d| d.join("config.toml"))
            .ok_or(ConfigError::NoHome)
    }

    fn figment(path: &Path) -> Figment {
        Figment::from(Serialized::defaults(Self::default()))
            .merge(Toml::file(path))
            .merge(Env::prefixed("CONSOLE_").ignore(RESERVED_ENV).split("__"))
    }

    /// The client API endpoint (`console acp` and the CLI connect here).
    #[must_use]
    pub fn client_url(&self) -> String {
        format!("ws://{}/client", self.orchestrator.listen)
    }

    /// The shared local token, created with mode 0600 on first use.
    pub fn token(&self) -> io::Result<String> {
        let path = self.data_dir.join("token");
        match std::fs::read_to_string(&path) {
            Ok(t) if !t.trim().is_empty() => return Ok(t.trim().to_owned()),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        std::fs::create_dir_all(&self.data_dir)?;
        let mut bytes = [0_u8; 32];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        write_private(&path, &token)?;
        Ok(token)
    }

    /// The configuration rendered as TOML, for `console config show`.
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("config always serializes")
    }
}

#[cfg(unix)]
fn write_private(path: &Path, contents: &str) -> io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(contents.as_bytes())
}

#[cfg(not(unix))]
fn write_private(path: &Path, contents: &str) -> io::Result<()> {
    std::fs::write(path, contents)
}

#[cfg(test)]
// `figment::Jail` closures return figment's large error type; fine in tests.
#[allow(clippy::result_large_err)]
mod tests {
    use super::*;
    use figment::Jail;

    #[test]
    fn defaults_match_the_plan() {
        let c = Config::default();
        assert_eq!(c.agent.model, "claude-opus-5-5");
        assert_eq!(c.agent.effort, Effort::Medium);
        assert_eq!(c.agent.inline_max_tool_calls, 10);
        assert_eq!(c.agent.inline_max_wall_secs, 90);
        assert_eq!(c.orchestrator.max_workers_per_workspace, 1);
        assert_eq!(c.orchestrator.max_workers_total, 2);
        assert!(c.executor.orchestrator_url.ends_with("/executor"));
    }

    #[test]
    fn missing_file_uses_defaults() {
        Jail::expect_with(|jail| {
            let c = Config::load(Some(&jail.directory().join("nope.toml"))).unwrap();
            assert_eq!(c.orchestrator, OrchestratorConfig::default());
            Ok(())
        });
    }

    #[test]
    fn file_then_env_override() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "c.toml",
                r#"
                data_dir = "/tmp/console-test"
                [agent]
                effort = "high"
                inline_max_tool_calls = 4
                [orchestrator]
                listen = "127.0.0.1:9000"
                "#,
            )?;
            jail.set_env("CONSOLE_AGENT__INLINE_MAX_TOOL_CALLS", "7");
            jail.set_env("CONSOLE_LOG__JSON", "true");
            let c = Config::load(Some(&jail.directory().join("c.toml"))).unwrap();
            assert_eq!(c.data_dir, PathBuf::from("/tmp/console-test"));
            assert_eq!(c.agent.effort, Effort::High);
            assert_eq!(c.agent.inline_max_tool_calls, 7, "env beats file");
            assert_eq!(c.orchestrator.listen.port(), 9000);
            assert!(c.log.json);
            assert_eq!(c.agent.model, "claude-opus-5-5", "unset keys keep defaults");
            Ok(())
        });
    }

    #[test]
    fn config_env_var_selects_file() {
        Jail::expect_with(|jail| {
            jail.create_file("alt.toml", "[log]\nfilter = \"debug\"\n")?;
            jail.set_env(
                CONFIG_ENV,
                jail.directory().join("alt.toml").display().to_string(),
            );
            let c = Config::load(None).unwrap();
            assert_eq!(c.log.filter, "debug");
            Ok(())
        });
    }

    #[test]
    fn hook_variables_are_not_config() {
        Jail::expect_with(|jail| {
            jail.set_env("CONSOLE_HOOK_URL", "ws://x");
            jail.set_env("CONSOLE_HOOK_TOKEN", "t");
            assert!(Config::load(Some(&jail.directory().join("none.toml"))).is_ok());
            Ok(())
        });
    }

    #[test]
    fn unknown_keys_are_errors() {
        Jail::expect_with(|jail| {
            jail.create_file("c.toml", "[agent]\nmodle = \"typo\"\n")?;
            let err = Config::load(Some(&jail.directory().join("c.toml"))).unwrap_err();
            assert!(err.to_string().contains("modle"), "{err}");
            Ok(())
        });
    }

    #[test]
    fn token_is_created_once_and_private() {
        let dir = tempfile::tempdir().unwrap();
        let c = Config {
            data_dir: dir.path().to_path_buf(),
            ..Config::default()
        };
        let a = c.token().unwrap();
        assert_eq!(a.len(), 64);
        assert_eq!(c.token().unwrap(), a, "stable across calls");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(dir.path().join("token"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn workspaces_and_harnesses_from_toml() {
        Jail::expect_with(|jail| {
            jail.create_file(
                "c.toml",
                r#"
                [executor]
                default_harness = "codex"
                [[executor.workspaces]]
                name = "console"
                path = "/src/console"
                "#,
            )?;
            let c = Config::load(Some(&jail.directory().join("c.toml"))).unwrap();
            assert_eq!(c.executor.workspaces[0].name, "console");
            assert_eq!(c.executor.default_harness, "codex");
            assert!(c.executor.harnesses.contains_key("claude"), "defaults kept");
            Ok(())
        });
    }

    #[test]
    fn toml_round_trip() {
        let c = Config::default();
        let parsed: Config = toml::from_str(&c.to_toml()).unwrap();
        assert_eq!(parsed, c);
    }
}
