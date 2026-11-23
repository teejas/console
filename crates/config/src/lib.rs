//! Layered configuration for every console component.
//!
//! Sources, lowest to highest precedence:
//! 1. Built-in defaults ([`Config::default`]).
//! 2. A TOML file: `--config <path>`, else `$CONSOLE_CONFIG`, else `<data_dir>/config.toml`.
//!    A missing file is fine; defaults apply.
//! 3. Environment variables prefixed `CONSOLE_`, with `__` separating nested keys
//!    (for example `CONSOLE_AGENT__EFFORT=high`).

use std::{
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
    /// Command that starts an ACP worker (Claude Code's ACP adapter), pinned, no npx.
    pub worker_command: Vec<String>,
    /// Commands the Orchestrator's `run_readonly` tool may run, by argv prefix.
    pub readonly_allowlist: Vec<Vec<String>>,
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
            worker_command: vec!["claude-agent-acp".into()],
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
            .merge(Env::prefixed("CONSOLE_").ignore(&["config"]).split("__"))
    }

    /// The configuration rendered as TOML, for `console config show`.
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("config always serializes")
    }
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
    fn unknown_keys_are_errors() {
        Jail::expect_with(|jail| {
            jail.create_file("c.toml", "[agent]\nmodle = \"typo\"\n")?;
            let err = Config::load(Some(&jail.directory().join("c.toml"))).unwrap_err();
            assert!(err.to_string().contains("modle"), "{err}");
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
