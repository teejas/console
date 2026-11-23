//! Turning Orchestrator events into ACP session updates. Pure, so it is unit-tested.

use std::collections::HashMap;

use agent_client_protocol::v1::schema::{
    ContentBlock, ContentChunk, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus, SessionUpdate,
    ToolCall, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use console_proto::api::TodoView;
use console_proto::domain::{Band, BlockedReason, TaskState, TodoStatus};
use console_proto::event::{Event, EventBody};
use console_proto::{TaskId, TurnId};

/// What the renderer remembers between events.
#[derive(Debug, Default)]
pub struct Renderer {
    pub titles: HashMap<TaskId, String>,
    /// Turns this bridge submitted; their user messages are already on screen.
    pub own_turns: std::collections::HashSet<TurnId>,
    /// The turn whose text was last written, to separate turns and notices.
    last_turn: Option<Option<TurnId>>,
}

pub fn text(t: impl Into<String>) -> SessionUpdate {
    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::from(t.into())))
}

fn user(t: impl Into<String>) -> SessionUpdate {
    SessionUpdate::UserMessageChunk(ContentChunk::new(ContentBlock::from(t.into())))
}

fn band_label(b: Band) -> &'static str {
    match b {
        Band::P0 => "P0",
        Band::P1 => "P1",
        Band::P2 => "P2",
        Band::P3 => "P3",
    }
}

/// The queue as an ACP plan, shown in Toad's plan panel.
#[must_use]
pub fn plan(queue: &[TodoView]) -> SessionUpdate {
    SessionUpdate::Plan(Plan::new(
        queue
            .iter()
            .map(|t| {
                let priority = match t.band {
                    Band::P0 | Band::P1 => PlanEntryPriority::High,
                    Band::P2 => PlanEntryPriority::Medium,
                    Band::P3 => PlanEntryPriority::Low,
                };
                let status = match t.status {
                    TodoStatus::Active => PlanEntryStatus::InProgress,
                    TodoStatus::Done | TodoStatus::Dropped => PlanEntryStatus::Completed,
                    _ => PlanEntryStatus::Pending,
                };
                let flag = match t.status {
                    TodoStatus::WaitingOnUser => " (needs you)",
                    TodoStatus::Blocked => " (blocked)",
                    _ => "",
                };
                PlanEntry::new(
                    format!("{} · {}{flag}", band_label(t.band), t.title),
                    priority,
                    status,
                )
            })
            .collect(),
    ))
}

impl Renderer {
    /// This bridge just submitted `turn`; its text starts a fresh message in the client.
    pub fn begin_turn(&mut self, turn: TurnId) {
        self.own_turns.insert(turn);
        self.last_turn = Some(Some(turn));
    }

    fn notice(&mut self, msg: String) -> Vec<SessionUpdate> {
        self.last_turn = Some(None);
        vec![text(format!("\n\n{msg}\n"))]
    }

    fn title(&self, id: TaskId) -> String {
        self.titles
            .get(&id)
            .cloned()
            .unwrap_or_else(|| id.to_string())
    }

    /// Renders one event. `replay` is true while rebuilding history on attach.
    pub fn render(&mut self, e: &Event, replay: bool) -> Vec<SessionUpdate> {
        match &e.body {
            EventBody::UserMessage { turn_id, text: t } => {
                if !replay && self.own_turns.contains(turn_id) {
                    return Vec::new();
                }
                self.last_turn = Some(Some(*turn_id));
                vec![user(t.clone())]
            }
            EventBody::AgentText { turn_id, text: t } => {
                // Start a new paragraph when the speaker's turn changes or after a notice.
                let mut out = Vec::new();
                if self.last_turn.is_some_and(|last| last != *turn_id) || turn_id.is_none() {
                    out.push(text("\n\n"));
                }
                self.last_turn = Some(*turn_id);
                out.push(text(t.clone()));
                out
            }
            EventBody::ToolCallStarted {
                tool_call_id,
                title,
                ..
            } if !replay => {
                vec![SessionUpdate::ToolCall(
                    ToolCall::new(tool_call_id.clone(), title.clone())
                        .status(ToolCallStatus::InProgress),
                )]
            }
            EventBody::ToolCallFinished {
                tool_call_id,
                ok,
                summary,
            } if !replay => {
                let mut fields = ToolCallUpdateFields::new().status(if *ok {
                    ToolCallStatus::Completed
                } else {
                    ToolCallStatus::Failed
                });
                if let Some(s) = summary {
                    fields = fields.content(vec![ContentBlock::from(s.clone()).into()]);
                }
                vec![SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    tool_call_id.clone(),
                    fields,
                ))]
            }
            EventBody::TaskDelegated { title, .. } => {
                if let Some(id) = e.task_id {
                    self.titles.insert(id, title.clone());
                }
                Vec::new()
            }
            EventBody::TaskTransition { to, blocked, .. } if !replay => {
                let Some(id) = e.task_id else {
                    return Vec::new();
                };
                let title = self.title(id);
                let msg = match (to, blocked) {
                    (TaskState::Running, None) => return Vec::new(),
                    (TaskState::Running, Some(BlockedReason::WaitingExecutor))
                    | (TaskState::Queued, Some(BlockedReason::WaitingExecutor)) => {
                        format!("⏸ \"{title}\" is waiting for its executor to come back online.")
                    }
                    (_, Some(_)) => return Vec::new(), // questions and decisions get their own notice
                    (TaskState::Succeeded, _) => format!(
                        "✓ \"{title}\" finished. Ask me for the details, or /diff to see the changes."
                    ),
                    (TaskState::Failed, _) => {
                        format!("✗ \"{title}\" failed. Ask me what happened.")
                    }
                    (TaskState::Cancelled, _) => format!("■ \"{title}\" was cancelled."),
                    (TaskState::OutcomeUnknown, _) => format!(
                        "? \"{title}\" was interrupted and its outcome is unknown. It won't be retried without you: ask me to check it or retry it."
                    ),
                    (TaskState::CancelRequested, _) => format!("Cancelling \"{title}\"…"),
                    (TaskState::Queued, None) => return Vec::new(),
                };
                self.notice(msg)
            }
            EventBody::DecisionRequested {
                question, options, ..
            } if !replay => {
                let title = e.task_id.map(|id| self.title(id)).unwrap_or_default();
                let how = if options.is_empty() {
                    "Reply with /answer and your answer.".to_owned()
                } else {
                    let opts: Vec<String> = options
                        .iter()
                        .map(|o| format!("`{}` ({})", o.id, o.label))
                        .collect();
                    format!(
                        "Options: {}. Reply with /answer and an option, or see /decisions.",
                        opts.join(", ")
                    )
                };
                self.notice(format!("❓ \"{title}\" needs you: {question}\n{how}"))
            }
            EventBody::ProposalOpened {
                field,
                value,
                rationale,
                ..
            } if !replay => self.notice(format!(
                "💡 Proposal: set {field} to {value} ({rationale}). See /proposals."
            )),
            EventBody::ExecutorStatus { online: false, .. } if !replay => self
                .notice("⚠ An executor went offline; work on it waits until it reconnects.".into()),
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use console_proto::EventSeq;

    fn ev(body: EventBody, task: Option<TaskId>) -> Event {
        Event {
            seq: EventSeq(1),
            at: Utc::now(),
            session_id: None,
            task_id: task,
            todo_id: None,
            body,
        }
    }

    fn texts(updates: &[SessionUpdate]) -> String {
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

    #[test]
    fn task_notices_use_titles_and_stand_apart() {
        let mut r = Renderer::default();
        let id = TaskId::generate();
        let ws = console_proto::WorkspaceId::generate();
        assert!(
            r.render(
                &ev(
                    EventBody::TaskDelegated {
                        title: "Fix flaky test".into(),
                        workspace_id: ws
                    },
                    Some(id)
                ),
                false
            )
            .is_empty()
        );
        let done = r.render(
            &ev(
                EventBody::TaskTransition {
                    from: TaskState::Running,
                    to: TaskState::Succeeded,
                    blocked: None,
                },
                Some(id),
            ),
            false,
        );
        let t = texts(&done);
        assert!(t.starts_with("\n\n✓ \"Fix flaky test\" finished"), "{t:?}");
        assert!(
            r.render(
                &ev(
                    EventBody::TaskTransition {
                        from: TaskState::Queued,
                        to: TaskState::Running,
                        blocked: None
                    },
                    Some(id)
                ),
                false
            )
            .is_empty()
        );
    }

    #[test]
    fn own_user_messages_are_not_echoed_live() {
        let mut r = Renderer::default();
        let turn = TurnId::generate();
        r.own_turns.insert(turn);
        let body = EventBody::UserMessage {
            turn_id: turn,
            text: "hi".into(),
        };
        assert!(r.render(&ev(body.clone(), None), false).is_empty());
        assert_eq!(
            r.render(&ev(body, None), true).len(),
            1,
            "replayed history shows it"
        );
    }

    #[test]
    fn decisions_explain_how_to_answer() {
        let mut r = Renderer::default();
        let d = r.render(
            &ev(
                EventBody::DecisionRequested {
                    decision_id: console_proto::DecisionId::generate(),
                    question: "Allow git push?".into(),
                    options: vec![console_proto::event::DecisionOption {
                        id: "allow".into(),
                        label: "Allow".into(),
                    }],
                },
                None,
            ),
            false,
        );
        let t = texts(&d);
        assert!(
            t.contains("Allow git push?") && t.contains("/answer"),
            "{t}"
        );
    }

    #[test]
    fn plan_maps_bands_and_status() {
        let todo = TodoView {
            id: console_proto::TodoId::generate(),
            title: "ship it".into(),
            kind: console_proto::domain::TodoKind::Feature,
            owner: console_proto::domain::TodoOwner::User,
            status: TodoStatus::WaitingOnUser,
            band: Band::P0,
            importance: console_proto::domain::Score::MAX,
            urgency: console_proto::domain::Score::MAX,
            due_at: None,
            why: String::new(),
            version: 1,
        };
        let SessionUpdate::Plan(p) = plan(&[todo]) else {
            panic!()
        };
        assert_eq!(p.entries[0].content, "P0 · ship it (needs you)");
        assert_eq!(p.entries[0].priority, PlanEntryPriority::High);
    }
}
