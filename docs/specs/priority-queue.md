# Shared priority queue

The todo list you and the Orchestrator both work from. Summary in [DESIGN.md](../../DESIGN.md).

## Todo

| Field | Owner | Notes |
|---|---|---|
| `title`, `kind` | Whoever created it | Kinds: feature, bugfix, review, investigation, ops, chore, question |
| `importance`, `urgency` | You (Orchestrator proposes) | Scores 1–4 |
| `due_at` | You | Optional |
| `owner` | You | `user` or `orchestrator`: who is expected to act |
| `status` | Shared | inbox, ready, active, blocked, waiting_on_user, done, dropped |
| `manual_rank` | You | Fractional index; set when you move an item |
| `workspace`, `task` links | Orchestrator | Set by delegation |
| `version` | System | Incremented on every change |

## Ordering

1. **Band** from importance × urgency. "High" is a score of 3 or more; a due date within 48 hours makes urgency high.

   | Band | Importance | Urgency | Examples |
   |---|---|---|---|
   | **P0** | High | High | Main broken, a teammate blocked on you, due today |
   | **P1** | Low | High | A quick review request, someone waiting on an answer |
   | **P2** | High | Low | Planned features, meaningful refactors, design work |
   | **P3** | Low | Low | Chores, cleanup, nice-to-haves |

   Urgent ranks above important so time-sensitive items clear first (confirmed November 22, 2026).
2. **Score within the band** from urgency, importance, items it unblocks, age, and a penalty for being blocked.
3. **Manual placement** (`manual_rank`) overrides the score within a band and sticks. Positions and scores share one 0–100 scale. Moving an item pins every item in that band at its current position, so the whole order you saw holds; new items still land by score. Moves across bands are refused: change importance or urgency instead.
4. **`waiting_on_user` items** are always listed first.
5. **`why`:** every item carries a one-line explanation of its position.

## Field ownership

- The Orchestrator never silently changes a field you own. It creates a **proposal** with a rationale, which you accept or reject; proposals expire if ignored.
- When the Orchestrator adds a todo, it proposes the kind, importance, urgency, and workspace in the same step, from the conversation. There is no separate triage call in the MVP.
- Every edit carries `expected_version`. A stale edit fails, and the client re-reads.

## Starting work

- In the MVP, work starts when you or the Orchestrator delegates. `/next` shows the top ready item, and the Orchestrator may suggest starting it.
- An autonomous scheduler, where the Orchestrator starts its own top ready items within the concurrency limits, comes later.

## Surfaces

- **Toad:** the top N as the plan panel; `/queue`, `/next`, `/why`, `/add`, `/proposals`.
- **CLI:** `console queue`, `console todo add|edit|move|done`.
- **Agent:** the queue tools in [orchestrator-agent.md](orchestrator-agent.md#tools), plus the queue top injected into every turn.
- **Inbound events** (later) add proposals to the inbox ([inbound-events.md](inbound-events.md)).

## Tests

Ranking, placement, and ownership are pure functions in the `queue` crate, with property tests: rank order is total and stable, manual placement survives score changes, and no Orchestrator path writes a field you own.
