//! Domain enums shared across components.

use serde::{Deserialize, Serialize};

/// Task-level outcome state (architecture §8.3). Blocked reasons sit beside the state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Running,
    CancelRequested,
    /// The outcome could not be verified (for example, the executor restarted mid-run).
    /// Resolved by reconciliation or an explicit user decision, never by a silent retry.
    OutcomeUnknown,
    Succeeded,
    Failed,
    Cancelled,
}

impl TaskState {
    /// Terminal states are absorbing: no transition leaves them.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

/// Why a non-terminal task is not making progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockedReason {
    /// The target executor is offline.
    WaitingExecutor,
    /// The worker asked a question (`ask_orchestrator`) or ended its turn without `complete`.
    AwaitingInput,
    /// A permission request or other decision is waiting on the user.
    AwaitingDecision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    /// The long-lived conversation with the Orchestrator.
    Orchestrator,
    /// A worker's session for one task.
    Worker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoKind {
    Feature,
    Bugfix,
    Review,
    Investigation,
    Ops,
    Chore,
    Question,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoOwner {
    User,
    Orchestrator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Inbox,
    Ready,
    Active,
    Blocked,
    WaitingOnUser,
    Done,
    Dropped,
}

/// Importance or urgency, scored 1 (lowest) to 4 (highest).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "u8", into = "u8")]
pub struct Score(u8);

impl Score {
    pub const MIN: Self = Self(1);
    pub const MAX: Self = Self(4);

    /// Scores of 3 or more count as "high" when assigning a band.
    #[must_use]
    pub const fn is_high(self) -> bool {
        self.0 >= 3
    }

    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl TryFrom<u8> for Score {
    type Error = String;

    fn try_from(v: u8) -> Result<Self, Self::Error> {
        if (1..=4).contains(&v) {
            Ok(Self(v))
        } else {
            Err(format!("score must be between 1 and 4, got {v}"))
        }
    }
}

impl From<Score> for u8 {
    fn from(s: Score) -> Self {
        s.0
    }
}

/// Priority band (`docs/specs/priority-queue.md`). Urgent items rank above important-but-not-urgent ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Band {
    /// Important and urgent.
    P0,
    /// Urgent, not important.
    P1,
    /// Important, not urgent.
    P2,
    /// Neither.
    P3,
}

impl Band {
    /// Assigns the band from scores. Due-date effects on urgency are applied by the queue
    /// before calling this.
    #[must_use]
    pub const fn from_scores(importance: Score, urgency: Score) -> Self {
        match (importance.is_high(), urgency.is_high()) {
            (true, true) => Self::P0,
            (false, true) => Self::P1,
            (true, false) => Self::P2,
            (false, false) => Self::P3,
        }
    }
}

/// Progressive-disclosure level (`docs/specs/progressive-disclosure.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DisclosureLevel {
    /// One-line headline.
    L0,
    /// A few bullets.
    L1,
    /// Full detail.
    L2,
    /// Raw evidence links.
    L3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PushMode {
    /// Deliver immediately, with a desktop notification.
    Now,
    /// Hold for the next `/sync`.
    Batch,
    /// Deliver only terminal digests.
    OnDone,
}

/// What the Orchestrator did with a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationDecision {
    Inline,
    Delegate,
    Ask,
}

/// A user override of the Orchestrator's inline-or-delegate choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOverride {
    Inline,
    Delegate,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: u8) -> Score {
        Score::try_from(v).unwrap()
    }

    #[test]
    fn bands_cover_all_score_combinations() {
        for i in 1..=4 {
            for u in 1..=4 {
                let expected = match (i >= 3, u >= 3) {
                    (true, true) => Band::P0,
                    (false, true) => Band::P1,
                    (true, false) => Band::P2,
                    (false, false) => Band::P3,
                };
                assert_eq!(
                    Band::from_scores(s(i), s(u)),
                    expected,
                    "importance={i} urgency={u}"
                );
            }
        }
    }

    #[test]
    fn urgent_ranks_above_important() {
        assert!(Band::P1 < Band::P2);
        assert_eq!(Band::from_scores(s(1), s(4)), Band::P1);
        assert_eq!(Band::from_scores(s(4), s(1)), Band::P2);
    }

    #[test]
    fn score_rejects_out_of_range() {
        assert!(Score::try_from(0).is_err());
        assert!(Score::try_from(5).is_err());
        assert!(serde_json::from_str::<Score>("7").is_err());
        assert_eq!(serde_json::from_str::<Score>("3").unwrap(), s(3));
    }

    #[test]
    fn terminal_states() {
        use TaskState::*;
        for st in [Queued, Running, CancelRequested, OutcomeUnknown] {
            assert!(!st.is_terminal(), "{st:?}");
        }
        for st in [Succeeded, Failed, Cancelled] {
            assert!(st.is_terminal(), "{st:?}");
        }
    }
}
