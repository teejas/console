-- The Orchestrator's store (docs/specs/state-and-durability.md).
-- Timestamps are RFC 3339 UTC strings; IDs are prefixed ULIDs.

-- Append-only event log. `seq` is the client cursor.
CREATE TABLE events (
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,
    at         TEXT NOT NULL,
    session_id TEXT,
    task_id    TEXT,
    todo_id    TEXT,
    kind       TEXT NOT NULL,
    payload    TEXT NOT NULL
);
CREATE INDEX events_by_session ON events (session_id, seq);
CREATE INDEX events_by_task ON events (task_id, seq);

CREATE TABLE sessions (
    id                  TEXT PRIMARY KEY,
    kind                TEXT NOT NULL,          -- orchestrator | worker
    task_id             TEXT,
    harness_session_ref TEXT,                   -- Claude Code session id, ACP session id
    title               TEXT NOT NULL,
    created_at          TEXT NOT NULL
);

CREATE TABLE turns (
    id              TEXT PRIMARY KEY,
    session_id      TEXT NOT NULL,
    idempotency_key TEXT NOT NULL UNIQUE,
    user_message    TEXT NOT NULL,
    status          TEXT NOT NULL,              -- pending | running | done | failed
    decision        TEXT,                       -- inline | delegate | ask
    reason          TEXT,
    tool_calls      INTEGER NOT NULL DEFAULT 0,
    wall_ms         INTEGER,
    budget_hit      INTEGER NOT NULL DEFAULT 0,
    override        TEXT,
    cost_usd        REAL,
    created_at      TEXT NOT NULL,
    ended_at        TEXT
);

CREATE TABLE executors (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    online       INTEGER NOT NULL DEFAULT 0,
    capabilities TEXT NOT NULL DEFAULT '[]',
    last_seen    TEXT NOT NULL
);

CREATE TABLE workspaces (
    id          TEXT PRIMARY KEY,
    executor_id TEXT NOT NULL,
    name        TEXT NOT NULL,
    path        TEXT NOT NULL,
    repo_remote TEXT,
    head_rev    TEXT,
    dirty       INTEGER NOT NULL DEFAULT 0,
    updated_at  TEXT NOT NULL
);

CREATE TABLE todos (
    id           TEXT PRIMARY KEY,
    title        TEXT NOT NULL,
    body         TEXT,
    kind         TEXT NOT NULL,
    owner        TEXT NOT NULL,
    status       TEXT NOT NULL,
    importance   INTEGER NOT NULL,
    urgency      INTEGER NOT NULL,
    due_at       TEXT,
    manual_rank  TEXT,                          -- fractional index, set when the user moves an item
    workspace_id TEXT,
    user_fields  TEXT NOT NULL DEFAULT '[]',    -- JSON array of fields the user has set
    version      INTEGER NOT NULL DEFAULT 1,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);

CREATE TABLE proposals (
    id         TEXT PRIMARY KEY,
    todo_id    TEXT NOT NULL,
    field      TEXT NOT NULL,
    value      TEXT NOT NULL,                   -- JSON
    rationale  TEXT NOT NULL,
    status     TEXT NOT NULL,                   -- open | accepted | rejected | expired
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL
);

CREATE TABLE tasks (
    id                TEXT PRIMARY KEY,
    todo_id           TEXT NOT NULL,
    title             TEXT NOT NULL,
    workspace_id      TEXT NOT NULL,
    harness           TEXT NOT NULL,
    origin_turn_id    TEXT,
    brief_artifact_id TEXT,
    worker_session_id TEXT,
    state             TEXT NOT NULL,
    blocked_reason    TEXT,
    version           INTEGER NOT NULL DEFAULT 1,
    idempotency_key   TEXT UNIQUE,
    summary           TEXT,
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL
);

CREATE TABLE attempts (
    id          TEXT PRIMARY KEY,
    task_id     TEXT NOT NULL,
    executor_id TEXT,
    created_at  TEXT NOT NULL,
    ended_at    TEXT,
    outcome     TEXT
);

CREATE TABLE commands (
    id          TEXT PRIMARY KEY,
    attempt_id  TEXT,
    task_id     TEXT,
    executor_id TEXT,
    op          TEXT NOT NULL,                  -- JSON
    status      TEXT NOT NULL,                  -- pending | accepted | running | succeeded | failed | cancelled | interrupted | unknown
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

CREATE TABLE decisions (
    id         TEXT PRIMARY KEY,
    task_id    TEXT NOT NULL,
    origin     TEXT NOT NULL,                   -- question | permission
    question   TEXT NOT NULL,
    options    TEXT NOT NULL,                   -- JSON array of {id, label}
    status     TEXT NOT NULL,                   -- open | resolved | cancelled
    option_id  TEXT,
    answer     TEXT,
    created_at TEXT NOT NULL,
    resolved_at TEXT
);

CREATE TABLE artifacts (
    id         TEXT PRIMARY KEY,
    task_id    TEXT,
    kind       TEXT NOT NULL,                   -- brief | patch
    content    TEXT NOT NULL,
    created_at TEXT NOT NULL
);
