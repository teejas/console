# Progressive disclosure

High level first, more detail on request, learned per kind of work. Summary in [DESIGN.md](../../DESIGN.md).

## MVP: by instruction

The Orchestrator's system prompt tells it to:
- lead with a one-line headline, then a few bullets at most;
- expand only when you ask ("more", "details", "show me the diff");
- always surface anything that needs you, whatever the level.

That needs no pipeline, and it shows whether the behavior is right before we build machinery around it.

## Later: digests and learning

### Levels

| Level | Content | Generated |
|---|---|---|
| L0 | One-line headline | Eagerly, at milestones |
| L1 | A few bullets: what changed, what's next, risks, whether it needs you | Eagerly, with L0 |
| L2 | Full detail | Lazily, on expand; cached |
| L3 | Raw evidence: events, patch, logs | On expand; links only, no model |

- **Milestones:** a task reaches a terminal state, a worker asks something, a decision is needed, a task is blocked or unblocked, and every ~10 minutes on long tasks.
- **Generation:** a headless Claude Code call with no tools, at effort `low`, validated against `{headline, needs_you, bullets, risks}` (about 7 s and $0.02 per digest in M0).
- **Grounding:** each digest records its event range, and deeper levels receive the shallower ones as input, so detail expands a summary rather than contradicting it.
- **Fallback:** a deterministic template digest if generation fails.

### Delivery

- **Push modes:** `now` (message plus desktop notification), `batch` (held for `/sync`), or `on_done` (terminal digests only).
- **Preference lookup:** `(kind, band) → (kind, *) → (*, band) → (*, *)`.
- **Overrides:** anything that needs you, and anything P0, is pushed now at L1 or deeper.

### Learning

- **Signals:** you expanded, collapsed, ignored, asked a follow-up the next level would have answered, or said what you want.
- **Rule:** move one level when 3 of the last 5 signals for a bucket agree.
- **Explicit statements are pinned** and never changed silently.
- **Every change is announced once** with its evidence and is visible in `/prefs`.

## Why it waits

The MVP proves the conversation, workers, and queue first. Digests need real task histories to tune against, and the event log already records what they will be grounded in.
