//! Logging setup. Logs always go to stderr: stdout is reserved for protocol traffic
//! (ACP in `console acp`, hook JSON in `console hook`).

use console_config::LogConfig;
use tracing_subscriber::{EnvFilter, fmt};

/// Installs the global subscriber. `RUST_LOG` overrides the configured filter.
pub fn init(config: &LogConfig) {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(&config.filter))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = fmt().with_env_filter(filter).with_writer(std::io::stderr);
    // A second init (for example in tests) is harmless; ignore the error.
    let _ = if config.json {
        builder.json().try_init()
    } else {
        builder.compact().try_init()
    };
}
