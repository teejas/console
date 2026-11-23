//! `console`: one binary for every role (Orchestrator, executor, ACP bridge, hooks) and
//! for the user's commands. Subcommands land milestone by milestone; ones that aren't
//! built yet say which milestone delivers them and exit with status 2.

mod telemetry;

use std::{path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
use console_config::Config;

#[derive(Debug, Parser)]
#[command(
    name = "console",
    version,
    about = "A persistent engineering assistant built around an Orchestrator."
)]
struct Cli {
    /// Config file (default: $CONSOLE_CONFIG, else ~/.console/config.toml).
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the Orchestrator service (daemon).
    Orchestrator,
    /// Run the executor daemon; it connects to the Orchestrator.
    Executor,
    /// ACP agent on stdio for Toad: the Orchestrator conversation, or a worker session.
    Acp {
        /// Attach to a worker's session instead of the Orchestrator conversation.
        #[arg(long, value_name = "TASK_ID")]
        task: Option<String>,
    },
    /// Claude Code hook entry points used by the Orchestrator agent.
    #[command(subcommand)]
    Hook(HookCommand),
    /// Install and start the daemons.
    Up,
    /// Stop the daemons.
    Down,
    /// Show daemon, executor, and workspace status.
    Status,
    /// Show the shared priority queue.
    Queue,
    /// Add, edit, move, or finish todos.
    Todo,
    /// List tasks.
    Tasks,
    /// Explain a task or turn: attempts, executor, waits, delegation decision, spend.
    Inspect { id: String },
    /// Show a task's logs.
    Logs { id: String },
    /// Answer a pending decision.
    Decide { id: String, option: String },
    /// Inspect configuration.
    #[command(subcommand)]
    Config(ConfigCommand),
}

#[derive(Debug, Subcommand)]
enum HookCommand {
    /// `UserPromptSubmit`: inject per-turn state.
    PromptSubmit,
    /// `PreToolUse`: enforce the inline budget.
    PreTool,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Print the effective configuration as TOML.
    Show,
    /// Print the config file path that would be read.
    Path,
}

fn not_yet(command: &str, milestone: &str) -> ExitCode {
    eprintln!("`console {command}` is not implemented yet (arrives in {milestone}).");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    // `config path` must work even when the file is invalid.
    if let Command::Config(ConfigCommand::Path) = cli.command {
        println!("{}", Config::file_path(cli.config.as_deref())?.display());
        return Ok(ExitCode::SUCCESS);
    }

    let config = Config::load(cli.config.as_deref())?;
    telemetry::init(&config.log);
    tracing::debug!(data_dir = %config.data_dir.display(), "config loaded");

    Ok(match cli.command {
        Command::Config(ConfigCommand::Show) => {
            print!("{}", config.to_toml());
            ExitCode::SUCCESS
        }
        Command::Config(ConfigCommand::Path) => unreachable!("handled above"),
        Command::Orchestrator => not_yet("orchestrator", "M2"),
        Command::Executor => not_yet("executor", "M3"),
        Command::Acp { .. } => not_yet("acp", "M6"),
        Command::Hook(_) => not_yet("hook", "M5"),
        Command::Up | Command::Down | Command::Status => not_yet("up/down/status", "M8"),
        Command::Queue | Command::Todo => not_yet("queue/todo", "M4"),
        Command::Tasks | Command::Decide { .. } => not_yet("tasks/decide", "M7"),
        Command::Inspect { .. } | Command::Logs { .. } => not_yet("inspect/logs", "M8"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
