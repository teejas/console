//! The task state machine (`docs/specs/state-and-durability.md#task-state-machine`).
//!
//! Pure: `apply(status, input)` returns the next status, the same status for a harmless
//! duplicate (the executor link is at-least-once), or `Illegal`. The engine writes the
//! result with a compare-and-set.

use console_proto::domain::{BlockedReason, TaskState};

/// A task's state plus the reason it isn't progressing, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub state: TaskState,
    pub blocked: Option<BlockedReason>,
}

impl Status {
    #[must_use]
    pub const fn new(state: TaskState, blocked: Option<BlockedReason>) -> Self {
        Self { state, blocked }
    }
    const fn plain(state: TaskState) -> Self {
        Self::new(state, None)
    }
}

/// Things that happen to a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// The executor journaled the `StartWorker` command.
    Accepted,
    /// The target executor went offline, or came back.
    ExecutorOffline,
    ExecutorOnline,
    /// The worker is waiting on the user (a question or a permission).
    Blocked(BlockedReason),
    /// The wait ended (the user answered).
    Unblocked,
    /// The worker finished (`complete`) or failed, as reported by the executor.
    Succeeded,
    Failed,
    /// The user asked to cancel.
    CancelRequested,
    /// The executor confirmed the worker stopped.
    CancelConfirmed,
    /// The executor lost track of the command (it restarted mid-run).
    OutcomeLost,
    /// Reconciliation or the user settled an unknown outcome.
    ReconciledSucceeded,
    ReconciledFailed,
    ReconciledCancelled,
    /// The user authorized a fresh attempt after an unknown outcome.
    RetryAuthorized,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{input:?} is not allowed while the task is {state:?}")]
pub struct Illegal {
    pub state: TaskState,
    pub input: Input,
}

/// Computes the next status. Returns `Ok(s)` unchanged for a no-op.
pub fn apply(s: Status, input: Input) -> Result<Status, Illegal> {
    use Input as I;
    use TaskState as T;
    let illegal = Err(Illegal {
        state: s.state,
        input,
    });
    let next = match (s.state, input) {
        // Terminal states are absorbing; repeating the same outcome is a no-op.
        (T::Succeeded, I::Succeeded | I::ReconciledSucceeded)
        | (T::Failed, I::Failed | I::ReconciledFailed)
        | (T::Cancelled, I::CancelConfirmed | I::CancelRequested | I::ReconciledCancelled) => s,
        (T::Succeeded | T::Failed | T::Cancelled, I::ExecutorOffline | I::ExecutorOnline) => s,
        (T::Succeeded | T::Failed | T::Cancelled, _) => return illegal,

        (T::Queued, I::Accepted) => Status::plain(T::Running),
        (T::Queued, I::ExecutorOffline) => {
            Status::new(T::Queued, Some(BlockedReason::WaitingExecutor))
        }
        (T::Queued, I::ExecutorOnline) => Status::plain(T::Queued),
        (T::Queued, I::CancelRequested) => Status::plain(T::Cancelled),
        // Dispatch was rejected outright.
        (T::Queued, I::Failed) => Status::plain(T::Failed),
        (T::Queued, _) => return illegal,

        (T::Running, I::Accepted) => s,
        (T::Running, I::Succeeded) => Status::plain(T::Succeeded),
        (T::Running, I::Failed) => Status::plain(T::Failed),
        (T::Running, I::CancelRequested) => Status::plain(T::CancelRequested),
        (T::Running, I::OutcomeLost) => Status::plain(T::OutcomeUnknown),
        (T::Running, I::Blocked(r)) => Status::new(T::Running, Some(r)),
        (T::Running, I::Unblocked) => Status::plain(T::Running),
        (T::Running, I::ExecutorOffline) => {
            Status::new(T::Running, Some(BlockedReason::WaitingExecutor))
        }
        (T::Running, I::ExecutorOnline) => match s.blocked {
            Some(BlockedReason::WaitingExecutor) => Status::plain(T::Running),
            _ => s,
        },
        (T::Running, _) => return illegal,

        (T::CancelRequested, I::CancelConfirmed) => Status::plain(T::Cancelled),
        (T::CancelRequested, I::Succeeded) => Status::plain(T::Succeeded),
        (T::CancelRequested, I::Failed) => Status::plain(T::Failed),
        (T::CancelRequested, I::OutcomeLost) => Status::plain(T::OutcomeUnknown),
        (
            T::CancelRequested,
            I::CancelRequested
            | I::Accepted
            | I::ExecutorOffline
            | I::ExecutorOnline
            | I::Blocked(_)
            | I::Unblocked,
        ) => s,
        (T::CancelRequested, _) => return illegal,

        (T::OutcomeUnknown, I::ReconciledSucceeded) => Status::plain(T::Succeeded),
        (T::OutcomeUnknown, I::ReconciledFailed) => Status::plain(T::Failed),
        (T::OutcomeUnknown, I::ReconciledCancelled | I::CancelRequested) => {
            Status::plain(T::Cancelled)
        }
        (T::OutcomeUnknown, I::RetryAuthorized) => Status::plain(T::Queued),
        (T::OutcomeUnknown, I::OutcomeLost | I::ExecutorOffline | I::ExecutorOnline) => s,
        (T::OutcomeUnknown, _) => return illegal,
    };
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use TaskState as T;
    use proptest::prelude::*;

    fn input() -> impl Strategy<Value = Input> {
        prop_oneof![
            Just(Input::Accepted),
            Just(Input::ExecutorOffline),
            Just(Input::ExecutorOnline),
            Just(Input::Blocked(BlockedReason::AwaitingInput)),
            Just(Input::Blocked(BlockedReason::AwaitingDecision)),
            Just(Input::Unblocked),
            Just(Input::Succeeded),
            Just(Input::Failed),
            Just(Input::CancelRequested),
            Just(Input::CancelConfirmed),
            Just(Input::OutcomeLost),
            Just(Input::ReconciledSucceeded),
            Just(Input::ReconciledFailed),
            Just(Input::ReconciledCancelled),
            Just(Input::RetryAuthorized),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(2000))]

        /// Random event sequences never violate the machine's invariants.
        #[test]
        fn invariants_hold(inputs in prop::collection::vec(input(), 0..40)) {
            let mut s = Status::plain(T::Queued);
            for i in inputs {
                let Ok(next) = apply(s, i) else { continue };
                // Terminal states never change.
                if s.state.is_terminal() {
                    prop_assert_eq!(next, s);
                }
                // Terminal states carry no blocked reason.
                if next.state.is_terminal() {
                    prop_assert_eq!(next.blocked, None);
                }
                // Running is entered only from Queued through an accepted attempt.
                if next.state == T::Running && s.state != T::Running {
                    prop_assert_eq!(s.state, T::Queued);
                    prop_assert_eq!(i, Input::Accepted);
                }
                // An unknown outcome is never silently retried or completed.
                if s.state == T::OutcomeUnknown && next.state != T::OutcomeUnknown {
                    prop_assert!(matches!(
                        i,
                        Input::ReconciledSucceeded
                            | Input::ReconciledFailed
                            | Input::ReconciledCancelled
                            | Input::CancelRequested
                            | Input::RetryAuthorized
                    ));
                }
                // Applying the same input twice is the same as applying it once,
                // so at-least-once redelivery is harmless.
                if let Ok(again) = apply(next, i)
                    && i != Input::RetryAuthorized
                    && i != Input::Accepted
                {
                    prop_assert_eq!(again, next);
                }
                s = next;
            }
        }
    }

    #[test]
    fn happy_path() {
        let mut s = Status::plain(T::Queued);
        for i in [
            Input::Accepted,
            Input::Blocked(BlockedReason::AwaitingInput),
            Input::Unblocked,
            Input::Succeeded,
        ] {
            s = apply(s, i).unwrap();
        }
        assert_eq!(s, Status::plain(T::Succeeded));
    }

    #[test]
    fn cancel_can_lose_to_completion() {
        let s = apply(Status::plain(T::Running), Input::CancelRequested).unwrap();
        assert_eq!(s.state, T::CancelRequested);
        assert_eq!(apply(s, Input::Succeeded).unwrap().state, T::Succeeded);
    }

    #[test]
    fn unknown_outcome_needs_a_decision() {
        let s = apply(Status::plain(T::Running), Input::OutcomeLost).unwrap();
        assert_eq!(s.state, T::OutcomeUnknown);
        assert!(
            apply(s, Input::Succeeded).is_err(),
            "a late result is not trusted silently"
        );
        assert_eq!(apply(s, Input::RetryAuthorized).unwrap().state, T::Queued);
    }

    #[test]
    fn offline_executor_blocks_and_recovers() {
        let s = apply(Status::plain(T::Queued), Input::ExecutorOffline).unwrap();
        assert_eq!(s.blocked, Some(BlockedReason::WaitingExecutor));
        assert_eq!(
            apply(s, Input::ExecutorOnline).unwrap(),
            Status::plain(T::Queued)
        );
    }
}
