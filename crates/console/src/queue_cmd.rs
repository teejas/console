//! `console queue` and `console todo`.

use anyhow::{Context as _, anyhow, bail};
use chrono::{DateTime, Duration, Utc};
use clap::{Args, Subcommand};
use console_client::Client;
use console_config::Config;
use console_proto::api::{
    Empty, ProposalList, ProposalResolve, ProposalResolveParams, ProposalView, QueueList,
    QueueListParams, TodoAdd, TodoAddParams, TodoMove, TodoMoveParams, TodoUpdate,
    TodoUpdateParams, TodoView,
};
use console_proto::domain::{Score, TodoKind, TodoStatus};

#[derive(Debug, Args)]
pub struct QueueArgs {
    /// Include done and dropped items.
    #[arg(long)]
    pub all: bool,
    /// Show why each item sits where it does.
    #[arg(long)]
    pub why: bool,
}

#[derive(Debug, Subcommand)]
pub enum TodoCommand {
    /// Add a todo. Scores you give are yours: the Orchestrator can only propose changes.
    Add {
        #[arg(required = true, num_args = 1..)]
        title: Vec<String>,
        #[arg(short, long, value_parser = parse_kind)]
        kind: Option<TodoKind>,
        /// Importance, 1 (low) to 4 (high).
        #[arg(short, long, value_parser = parse_score)]
        importance: Option<Score>,
        /// Urgency, 1 (low) to 4 (high).
        #[arg(short, long, value_parser = parse_score)]
        urgency: Option<Score>,
        /// Due date: RFC 3339, or relative like `48h` or `3d`.
        #[arg(long, value_parser = parse_due)]
        due: Option<DateTime<Utc>>,
    },
    /// Change a todo's fields.
    Edit {
        /// Queue number, or a unique suffix of the ID.
        todo: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(short, long, value_parser = parse_kind)]
        kind: Option<TodoKind>,
        #[arg(short, long, value_parser = parse_score)]
        importance: Option<Score>,
        #[arg(short, long, value_parser = parse_score)]
        urgency: Option<Score>,
        #[arg(long, value_parser = parse_due, conflicts_with = "no_due")]
        due: Option<DateTime<Utc>>,
        /// Remove the due date.
        #[arg(long)]
        no_due: bool,
    },
    /// Move a todo within its band; the band's order then sticks.
    Move {
        todo: String,
        /// Place it directly above this item.
        #[arg(long, conflicts_with = "below", required_unless_present = "below")]
        above: Option<String>,
        /// Place it directly below this item.
        #[arg(long)]
        below: Option<String>,
    },
    /// Mark a todo done.
    Done { todo: String },
    /// Drop a todo.
    Drop { todo: String },
    /// List the Orchestrator's open proposals.
    Proposals,
    /// Accept a proposal (by its number in `console todo proposals`).
    Accept { proposal: usize },
    /// Reject a proposal.
    Reject { proposal: usize },
}

fn parse_score(s: &str) -> Result<Score, String> {
    let n: u8 = s
        .parse()
        .map_err(|_| format!("{s:?} is not a number from 1 to 4"))?;
    Score::try_from(n)
}

fn parse_kind(s: &str) -> Result<TodoKind, String> {
    serde_json::from_value(serde_json::Value::String(s.to_lowercase())).map_err(|_| {
        "expected one of: feature, bugfix, review, investigation, ops, chore, question".to_owned()
    })
}

fn parse_due(s: &str) -> Result<DateTime<Utc>, String> {
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t.with_timezone(&Utc));
    }
    let (num, unit) = s.split_at(s.len().saturating_sub(1));
    let n: i64 = num
        .parse()
        .map_err(|_| format!("{s:?}: use RFC 3339 or a relative time like 48h or 3d"))?;
    match unit {
        "h" => Ok(Utc::now() + Duration::hours(n)),
        "d" => Ok(Utc::now() + Duration::days(n)),
        _ => Err(format!(
            "{s:?}: use RFC 3339 or a relative time like 48h or 3d"
        )),
    }
}

pub async fn connect(config: &Config) -> anyhow::Result<Client> {
    let token = config.token().context("reading the local token")?;
    let (client, _) = Client::connect(&config.client_url(), &token)
        .await
        .context("is the Orchestrator running? (`console orchestrator`)")?;
    Ok(client)
}

fn enum_str<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn short(id: &impl std::fmt::Display) -> String {
    let s = id.to_string();
    s[s.len().saturating_sub(6)..].to_lowercase()
}

pub async fn queue(config: &Config, args: QueueArgs) -> anyhow::Result<()> {
    let client = connect(config).await?;
    let items = client
        .call::<QueueList>(QueueListParams {
            include_done: args.all,
        })
        .await?;
    if items.is_empty() {
        println!("The queue is empty. Add something with `console todo add`.");
        return Ok(());
    }
    for (i, t) in items.iter().enumerate() {
        let due = t
            .due_at
            .map(|d| format!("  due {}", d.format("%b %-d %H:%M")))
            .unwrap_or_default();
        println!(
            "{:>3}  {:?}  {:<15} {}{due}  [{}]",
            i + 1,
            t.band,
            enum_str(&t.status),
            t.title,
            short(&t.id)
        );
        if args.why {
            println!("       {}", t.why);
        }
    }
    Ok(())
}

/// Resolves a queue number or ID suffix to a todo.
async fn find(client: &Client, reference: &str) -> anyhow::Result<TodoView> {
    let open = client.call::<QueueList>(QueueListParams::default()).await?;
    if let Ok(n) = reference.parse::<usize>()
        && n >= 1
        && n <= open.len()
    {
        return Ok(open[n - 1].clone());
    }
    let all = client
        .call::<QueueList>(QueueListParams { include_done: true })
        .await?;
    let needle = reference.to_lowercase();
    let matches: Vec<_> = all
        .into_iter()
        .filter(|t| t.id.to_string().to_lowercase().ends_with(&needle))
        .collect();
    match matches.len() {
        1 => Ok(matches.into_iter().next().unwrap_or_else(|| unreachable!())),
        0 => bail!("no todo matches {reference:?}"),
        n => bail!("{reference:?} matches {n} todos; use more of the ID"),
    }
}

fn blank_update(t: &TodoView) -> TodoUpdateParams {
    TodoUpdateParams {
        todo_id: t.id,
        expected_version: t.version,
        title: None,
        kind: None,
        owner: None,
        status: None,
        importance: None,
        urgency: None,
        due_at: None,
        clear_due: false,
        workspace_id: None,
    }
}

fn print_todo(verb: &str, t: &TodoView) {
    println!(
        "{verb} [{}] {:?} {}: {}",
        short(&t.id),
        t.band,
        t.title,
        t.why
    );
}

async fn proposals(client: &Client) -> anyhow::Result<Vec<ProposalView>> {
    Ok(client.call::<ProposalList>(Empty {}).await?)
}

pub async fn todo(config: &Config, cmd: TodoCommand) -> anyhow::Result<()> {
    let client = connect(config).await?;
    match cmd {
        TodoCommand::Add {
            title,
            kind,
            importance,
            urgency,
            due,
        } => {
            let t = client
                .call::<TodoAdd>(TodoAddParams {
                    title: title.join(" "),
                    body: None,
                    kind,
                    importance,
                    urgency,
                    due_at: due,
                    workspace_id: None,
                })
                .await?;
            print_todo("Added", &t);
        }
        TodoCommand::Edit {
            todo,
            title,
            kind,
            importance,
            urgency,
            due,
            no_due,
        } => {
            let t = find(&client, &todo).await?;
            let mut p = blank_update(&t);
            p.title = title;
            p.kind = kind;
            p.importance = importance;
            p.urgency = urgency;
            p.due_at = due;
            p.clear_due = no_due;
            let r = client.call::<TodoUpdate>(p).await?;
            print_todo("Updated", &r.todo);
        }
        TodoCommand::Move { todo, above, below } => {
            let t = find(&client, &todo).await?;
            let above = match above {
                Some(r) => Some(find(&client, &r).await?.id),
                None => None,
            };
            let below = match below {
                Some(r) => Some(find(&client, &r).await?.id),
                None => None,
            };
            let r = client
                .call::<TodoMove>(TodoMoveParams {
                    todo_id: t.id,
                    before: above,
                    after: below,
                    expected_version: t.version,
                })
                .await?;
            print_todo("Moved", &r);
        }
        TodoCommand::Done { todo } => set_status(&client, &todo, TodoStatus::Done, "Done").await?,
        TodoCommand::Drop { todo } => {
            set_status(&client, &todo, TodoStatus::Dropped, "Dropped").await?
        }
        TodoCommand::Proposals => {
            let ps = proposals(&client).await?;
            if ps.is_empty() {
                println!("No open proposals.");
            }
            for (i, p) in ps.iter().enumerate() {
                println!(
                    "{:>3}  {}: set {} to {} ({})",
                    i + 1,
                    p.todo_title,
                    p.field,
                    p.value,
                    p.rationale
                );
            }
        }
        TodoCommand::Accept { proposal } | TodoCommand::Reject { proposal } if proposal == 0 => {
            bail!("proposal numbers start at 1");
        }
        TodoCommand::Accept { proposal } => resolve(&client, proposal, true).await?,
        TodoCommand::Reject { proposal } => resolve(&client, proposal, false).await?,
    }
    Ok(())
}

async fn set_status(
    client: &Client,
    reference: &str,
    status: TodoStatus,
    verb: &str,
) -> anyhow::Result<()> {
    let t = find(client, reference).await?;
    let mut p = blank_update(&t);
    p.status = Some(status);
    let r = client.call::<TodoUpdate>(p).await?;
    print_todo(verb, &r.todo);
    Ok(())
}

async fn resolve(client: &Client, n: usize, accept: bool) -> anyhow::Result<()> {
    let ps = proposals(client).await?;
    let p = ps
        .get(n - 1)
        .ok_or_else(|| anyhow!("no proposal {n}; see `console todo proposals`"))?;
    let t = client
        .call::<ProposalResolve>(ProposalResolveParams {
            proposal_id: p.id,
            accept,
        })
        .await?;
    print_todo(if accept { "Accepted" } else { "Rejected" }, &t);
    Ok(())
}

/// Sends one message and prints the Orchestrator's reply as it streams.
pub async fn ask(config: &Config, message: &str) -> anyhow::Result<()> {
    use console_proto::api::{
        AttachParams, EventsSubscribe, SessionAttach, SessionTarget, SubscribeParams, TurnSubmit,
        TurnSubmitParams,
    };
    use console_proto::event::EventBody;
    use std::io::Write as _;
    let token = config.token().context("reading the local token")?;
    let (client, mut events) = Client::connect(&config.client_url(), &token)
        .await
        .context("is the Orchestrator running? (`console orchestrator`)")?;
    let attached = client
        .call::<SessionAttach>(AttachParams {
            target: SessionTarget::Orchestrator,
            recent: 0,
        })
        .await?;
    client
        .call::<EventsSubscribe>(SubscribeParams {
            session_id: attached.session_id,
            after: attached.cursor,
        })
        .await?;
    let key = format!("ask-{}", console_proto::TurnId::generate());
    let turn = client
        .call::<TurnSubmit>(TurnSubmitParams {
            text: message.to_owned(),
            idempotency_key: key,
        })
        .await?
        .turn_id;
    let mut out = std::io::stdout();
    while let Some(n) = events.recv().await {
        match n.event.body {
            EventBody::AgentText { turn_id, text } if turn_id == Some(turn) => {
                let _ = write!(out, "{text}");
                let _ = out.flush();
            }
            EventBody::ToolCallStarted { turn_id, title, .. } if turn_id == Some(turn) => {
                eprintln!("\n  ↳ {title}");
            }
            EventBody::TurnEnded {
                turn_id,
                decision,
                tool_calls,
                wall_ms,
                ..
            } if turn_id == turn => {
                println!();
                eprintln!(
                    "  ({} · {tool_calls} tool calls · {:.1}s)",
                    enum_str(&decision),
                    wall_ms as f64 / 1000.0
                );
                break;
            }
            _ => {}
        }
    }
    Ok(())
}

/// `console tasks`: delegated work and where it stands.
pub async fn tasks(config: &Config, all: bool) -> anyhow::Result<()> {
    use console_proto::api::{TaskList, TaskListParams};
    let client = connect(config).await?;
    let tasks = client
        .call::<TaskList>(TaskListParams { include_done: all })
        .await?;
    if tasks.is_empty() {
        println!("No {}tasks.", if all { "" } else { "active " });
    }
    for (i, t) in tasks.iter().enumerate() {
        let blocked = t
            .blocked
            .map(|b| format!(" ({})", enum_str(&b)))
            .unwrap_or_default();
        println!(
            "{:>3}  {:<16} {}{blocked}  [{}]",
            i + 1,
            enum_str(&t.state),
            t.title,
            short(&t.id)
        );
    }
    Ok(())
}

/// `console decide`: list open decisions, or answer one.
pub async fn decide(config: &Config, n: Option<usize>, answer: &str) -> anyhow::Result<()> {
    use console_proto::api::{
        AttachParams, DecisionRespond, DecisionRespondParams, SessionAttach, SessionTarget,
    };
    let client = connect(config).await?;
    let open = client
        .call::<SessionAttach>(AttachParams {
            target: SessionTarget::Orchestrator,
            recent: 0,
        })
        .await?
        .open_decisions;
    let Some(n) = n else {
        if open.is_empty() {
            println!("Nothing is waiting on you.");
        }
        for (i, d) in open.iter().enumerate() {
            let opts: Vec<&str> = d.options.iter().map(|o| o.id.as_str()).collect();
            let how = if opts.is_empty() {
                "free text".to_owned()
            } else {
                opts.join(" / ")
            };
            println!("{:>3}  {}  [{how}]", i + 1, d.question);
        }
        return Ok(());
    };
    let d = open
        .get(n.wrapping_sub(1))
        .ok_or_else(|| anyhow!("no decision {n}; run `console decide`"))?;
    if answer.trim().is_empty() {
        bail!("give an option or an answer: `console decide {n} <answer>`");
    }
    let (option_id, text) = if d.options.is_empty() {
        ("answer".to_owned(), Some(answer.to_owned()))
    } else {
        let o = d
            .options
            .iter()
            .find(|o| o.id.eq_ignore_ascii_case(answer) || o.label.eq_ignore_ascii_case(answer))
            .ok_or_else(|| {
                anyhow!(
                    "choose one of: {}",
                    d.options
                        .iter()
                        .map(|o| o.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
        (o.id.clone(), None)
    };
    client
        .call::<DecisionRespond>(DecisionRespondParams {
            decision_id: d.id,
            option_id,
            expected_state: None,
            answer: text,
        })
        .await?;
    println!("Answered.");
    Ok(())
}
