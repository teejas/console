//! Task engine: applies state-machine inputs to stored tasks with compare-and-set, and
//! keeps the linked todo's status in step.

use console_proto::TaskId;
use console_proto::domain::{Band, BlockedReason, TaskState, TodoStatus};
use console_proto::event::EventBody;
use console_store::{EventKeys, Store, StoreError, TaskRecord, WriteTx};

use crate::task_machine::{self, Illegal, Input, Status};

const MAX_RETRIES: usize = 5;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Illegal(#[from] Illegal),
    #[error("task {0} not found")]
    NotFound(TaskId),
}

/// Applies `input` to a task. Retries on write conflicts; returns the task as stored.
/// A no-op input (for example a redelivered result) returns the task unchanged.
pub async fn apply(
    store: &Store,
    task_id: TaskId,
    input: Input,
) -> Result<TaskRecord, EngineError> {
    for _ in 0..MAX_RETRIES {
        let task = store
            .task(task_id)
            .await?
            .ok_or(EngineError::NotFound(task_id))?;
        let current = Status::new(task.state, task.blocked);
        let next = task_machine::apply(current, input)?;
        if next == current {
            return Ok(task);
        }
        match store
            .write(async |tx| {
                let updated = tx
                    .transition_task(task.id, task.state, task.version, next.state, next.blocked)
                    .await?;
                after_transition(tx, &updated).await?;
                Ok(updated)
            })
            .await
        {
            Ok(updated) => {
                tracing::info!(task = %task_id, from = ?task.state, to = ?updated.state, blocked = ?updated.blocked, "task transition");
                return Ok(updated);
            }
            Err(StoreError::Conflict(_)) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(StoreError::Conflict(format!("{task_id}: too many concurrent updates")).into())
}

/// Side effects that belong in the same transaction as a transition.
async fn after_transition(tx: &mut WriteTx, task: &TaskRecord) -> Result<(), StoreError> {
    if task.state.is_terminal() {
        tx.cancel_decisions_for(task.id).await?;
    }
    sync_todo(tx, task).await
}

/// The todo status that mirrors a task's status.
#[must_use]
pub fn todo_status_for(state: TaskState, blocked: Option<BlockedReason>) -> TodoStatus {
    match (state, blocked) {
        (TaskState::Succeeded, _) => TodoStatus::Done,
        (TaskState::Cancelled, _) => TodoStatus::Ready,
        (TaskState::Failed | TaskState::OutcomeUnknown, _) => TodoStatus::WaitingOnUser,
        (_, Some(BlockedReason::AwaitingInput | BlockedReason::AwaitingDecision)) => {
            TodoStatus::WaitingOnUser
        }
        (_, Some(BlockedReason::WaitingExecutor)) => TodoStatus::Blocked,
        (TaskState::Queued, None) => TodoStatus::Ready,
        (TaskState::Running | TaskState::CancelRequested, None) => TodoStatus::Active,
    }
}

async fn sync_todo(tx: &mut WriteTx, task: &TaskRecord) -> Result<(), StoreError> {
    let mut todo = match tx.todo(task.todo_id).await {
        Ok(t) => t,
        // Tasks created without a stored todo (tests, older data) have nothing to sync.
        Err(StoreError::NotFound(_)) => return Ok(()),
        Err(e) => return Err(e),
    };
    let status = todo_status_for(task.state, task.blocked);
    if todo.status == status {
        return Ok(());
    }
    todo.status = status;
    let todo = tx.update_todo(&todo).await?;
    tx.append(
        EventKeys::todo(todo.id).with_task(task.id),
        EventBody::TodoChanged {
            title: todo.title.clone(),
            band: Band::from_scores(todo.importance, todo.urgency),
            status: todo.status,
            version: todo.version,
        },
    )
    .await?;
    Ok(())
}
