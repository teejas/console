//! `console`: one binary for every role (Orchestrator, executor, ACP bridge, hooks) and
//! for the user's commands. Subcommands land milestone by milestone; ones that aren't
//! built yet say which milestone delivers them and exit with status 2.

mod queue_cmd;
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
    /// Send one message to the Orchestrator and print its reply.
    Ask {
        #[arg(required = true, num_args = 1..)]
        message: Vec<String>,
    },
    /// Show the shared priority queue.
    Queue(queue_cmd::QueueArgs),
    /// Add, edit, move, or finish todos, and answer the Orchestrator's proposals.
    #[command(subcommand)]
    Todo(queue_cmd::TodoCommand),
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

/// Relays a Claude Code hook payload to the Orchestrator and prints its answer. Fails open:
/// if the service can't be reached, the hook prints nothing and the call proceeds.
async fn run_hook(config: &Config, method: &str) {
    use std::io::Read as _;
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let payload: serde_json::Value = serde_json::from_str(&input).unwrap_or_default();
    let url = std::env::var("CONSOLE_HOOK_URL").unwrap_or_else(|_| config.client_url());
    let token = match std::env::var("CONSOLE_HOOK_TOKEN") {
        Ok(t) => t,
        Err(_) => match config.token() {
            Ok(t) => t,
            Err(_) => return,
        },
    };
    let Ok((client, _)) = console_client::Client::connect(&url, &token).await else {
        return;
    };
    if let Ok(out) = client.call_raw(method, payload).await
        && out.as_object().is_some_and(|o| !o.is_empty())
    {
        println!("{out}");
    }
}

fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
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

    // Hooks must never fail on a config problem: they fall back to defaults and env.
    let config = match (Config::load(cli.config.as_deref()), &cli.command) {
        (Ok(c), _) => c,
        (Err(_), Command::Hook(_)) => Config::default(),
        (Err(e), _) => return Err(e.into()),
    };
    telemetry::init(&config.log);
    tracing::debug!(data_dir = %config.data_dir.display(), "config loaded");

    Ok(match cli.command {
        Command::Config(ConfigCommand::Show) => {
            print!("{}", config.to_toml());
            ExitCode::SUCCESS
        }
        Command::Config(ConfigCommand::Path) => unreachable!("handled above"),
        Command::Orchestrator => {
            runtime()?.block_on(console_orchestrator::run(config))?;
            ExitCode::SUCCESS
        }
        Command::Executor => {
            runtime()?.block_on(console_executor::run(config))?;
            ExitCode::SUCCESS
        }
        Command::Acp { task } => {
            let token = config.token()?;
            runtime()?.block_on(console_acp_bridge::run(&config.client_url(), &token, task))?;
            ExitCode::SUCCESS
        }
        Command::Hook(hook) => {
            let method = match hook {
                HookCommand::PromptSubmit => console_orchestrator::HOOK_PROMPT_SUBMIT,
                HookCommand::PreTool => console_orchestrator::HOOK_PRE_TOOL,
            };
            runtime()?.block_on(run_hook(&config, method));
            ExitCode::SUCCESS
        }
        Command::Up | Command::Down | Command::Status => not_yet("up/down/status", "M8"),
        Command::Ask { message } => {
            runtime()?.block_on(queue_cmd::ask(&config, &message.join(" ")))?;
            ExitCode::SUCCESS
        }
        Command::Queue(args) => {
            runtime()?.block_on(queue_cmd::queue(&config, args))?;
            ExitCode::SUCCESS
        }
        Command::Todo(cmd) => {
            runtime()?.block_on(queue_cmd::todo(&config, cmd))?;
            ExitCode::SUCCESS
        }
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
