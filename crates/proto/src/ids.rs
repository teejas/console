//! Typed identifiers.
//!
//! Every durable entity gets a ULID-backed newtype so IDs can't be mixed up. On the wire
//! and in logs they render as `<prefix>_<ULID>` (for example `task_01KAQ3...`), which keeps
//! them sortable by creation time and readable at a glance.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use ulid::Ulid;

/// An identifier string that failed to parse.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    #[error("expected an id starting with `{expected}_`, got `{got}`")]
    WrongPrefix { expected: &'static str, got: String },
    #[error("invalid ULID in id `{0}`")]
    InvalidUlid(String),
}

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident, $prefix:literal) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(Ulid);

        impl $name {
            /// The string prefix used on the wire.
            pub const PREFIX: &'static str = $prefix;

            /// A new, unique ID stamped with the current time.
            #[must_use]
            pub fn generate() -> Self {
                Self(Ulid::generate())
            }

            /// Wraps an existing ULID (used by tests and the store).
            #[must_use]
            pub const fn from_ulid(ulid: Ulid) -> Self {
                Self(ulid)
            }

            /// The underlying ULID.
            #[must_use]
            pub const fn ulid(&self) -> Ulid {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}_{}", $prefix, self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(self, f)
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                let rest = s
                    .strip_prefix($prefix)
                    .and_then(|r| r.strip_prefix('_'))
                    .ok_or_else(|| IdError::WrongPrefix { expected: $prefix, got: s.to_string() })?;
                Ulid::from_string(rest)
                    .map(Self)
                    .map_err(|_| IdError::InvalidUlid(s.to_string()))
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let s = String::deserialize(deserializer)?;
                s.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

id_type!(
    /// A conversation clients attach to: the Orchestrator conversation or a worker session.
    SessionId,
    "ses"
);
id_type!(
    /// One user message and the Orchestrator's response to it.
    TurnId,
    "turn"
);
id_type!(
    /// An item in the shared priority queue.
    TodoId,
    "todo"
);
id_type!(
    /// A unit of delegated work, executed by a worker.
    TaskId,
    "task"
);
id_type!(
    /// One execution attempt of a task. Retries create new attempts.
    AttemptId,
    "att"
);
id_type!(
    /// A dispatched operation, used for deduplication and reconciliation.
    CommandId,
    "cmd"
);
id_type!(
    /// A registered execution environment (the laptop, later cloud workers).
    ExecutorId,
    "exe"
);
id_type!(
    /// A repository checkout scoped to one executor.
    WorkspaceId,
    "ws"
);
id_type!(
    /// A versioned output: brief, patch, log bundle, transcript.
    ArtifactId,
    "art"
);
id_type!(
    /// A question or permission request waiting on the user.
    DecisionId,
    "dec"
);
id_type!(
    /// A layered progress summary.
    DigestId,
    "dig"
);
id_type!(
    /// A proposed change to user-owned queue fields.
    ProposalId,
    "prop"
);
id_type!(
    /// A PTY session owned by an executor.
    TerminalId,
    "term"
);

/// Position in the event log. Strictly increasing; clients resume after a cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventSeq(pub u64);

impl EventSeq {
    /// The cursor before the first event.
    pub const START: Self = Self(0);
}

/// Generation of the active Orchestrator authority. Executors reject dispatches from a
/// lower epoch, which fences off a stale installation after a migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AuthorityEpoch(pub u32);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_and_parse_round_trip() {
        let id = TaskId::generate();
        let s = id.to_string();
        assert!(s.starts_with("task_"));
        assert_eq!(s.parse::<TaskId>().unwrap(), id);
    }

    #[test]
    fn wrong_prefix_is_rejected() {
        let s = TodoId::generate().to_string();
        assert!(matches!(
            s.parse::<TaskId>(),
            Err(IdError::WrongPrefix { .. })
        ));
    }

    #[test]
    fn prefix_must_be_followed_by_underscore() {
        assert!(
            "taskx01KAQ3Z8P4V0000000000000000"
                .parse::<TaskId>()
                .is_err()
        );
    }

    #[test]
    fn invalid_ulid_is_rejected() {
        assert!(matches!(
            "task_nope".parse::<TaskId>(),
            Err(IdError::InvalidUlid(_))
        ));
    }

    #[test]
    fn serde_uses_prefixed_string() {
        let id = SessionId::generate();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{id}\""));
        assert_eq!(serde_json::from_str::<SessionId>(&json).unwrap(), id);
    }

    #[test]
    fn ids_sort_by_creation_time() {
        let a = TaskId::from_ulid(Ulid::from_parts(1_000, 0));
        let b = TaskId::from_ulid(Ulid::from_parts(2_000, 0));
        assert!(a < b);
    }
}
