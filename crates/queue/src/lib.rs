//! Shared priority queue: bands, ranking, manual placement, proposals, field ownership.
//! Pure logic, no IO (M4).
//!
//! See `docs/specs/priority-queue.md`.
//!
//! - **Band** from importance × urgency, where a due date within 48 hours makes urgency
//!   high and an overdue item is maximally urgent.
//! - **Within a band**, items sort by `position`: the user's manual placement if they set
//!   one, else the computed score. Both are on the same 0–100 scale, so a placed item keeps
//!   its spot while new items still land by score around it.
//! - **`waiting_on_user`** items always come first.
//! - **Ownership:** fields the user set change only through proposals from the Orchestrator.

use chrono::{DateTime, Duration, Utc};
use console_proto::domain::{Band, Score, TodoKind, TodoOwner, TodoStatus};
use console_proto::{TodoId, WorkspaceId};

/// The subset of a todo that ranking and ownership need.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub id: TodoId,
    pub title: String,
    pub kind: TodoKind,
    pub owner: TodoOwner,
    pub status: TodoStatus,
    pub importance: Score,
    pub urgency: Score,
    pub due_at: Option<DateTime<Utc>>,
    /// Manual placement on the 0–100 position scale.
    pub manual_rank: Option<f64>,
    pub workspace_id: Option<WorkspaceId>,
    pub user_fields: Vec<String>,
    pub created_at: DateTime<Utc>,
    /// How many other items depend on this one.
    pub unblocks: u32,
}

const DUE_SOON: i64 = 48;

fn score(v: u8) -> Score {
    Score::try_from(v.clamp(1, 4)).unwrap_or(Score::MIN)
}

/// Urgency after due-date effects.
#[must_use]
pub fn effective_urgency(item: &Item, now: DateTime<Utc>) -> Score {
    match item.due_at {
        Some(due) if due <= now => Score::MAX,
        Some(due) if due - now <= Duration::hours(DUE_SOON) => score(item.urgency.get().max(3)),
        _ => item.urgency,
    }
}

#[must_use]
pub fn band(item: &Item, now: DateTime<Utc>) -> Band {
    Band::from_scores(item.importance, effective_urgency(item, now))
}

/// The computed in-band score, 0–100.
#[must_use]
pub fn computed_score(item: &Item, now: DateTime<Utc>) -> f64 {
    let urgency = f64::from(effective_urgency(item, now).get());
    let importance = f64::from(item.importance.get());
    let unblocks = f64::from(item.unblocks.min(3)) * 5.0;
    let age_days = (now - item.created_at).num_hours().max(0) as f64 / 24.0;
    let age = age_days.min(10.0);
    let blocked = if item.status == TodoStatus::Blocked {
        30.0
    } else {
        0.0
    };
    // 1–4 scores map to 10–40 each; the rest are small adjustments.
    (urgency * 10.0 + importance * 10.0 + unblocks + age - blocked).clamp(0.0, 100.0)
}

/// Where an item sits within its band: manual placement wins over the computed score.
#[must_use]
pub fn position(item: &Item, now: DateTime<Utc>) -> f64 {
    item.manual_rank
        .unwrap_or_else(|| computed_score(item, now))
}

/// A ranked item with its explanation.
#[derive(Debug, Clone, PartialEq)]
pub struct Ranked {
    pub item: Item,
    pub band: Band,
    pub position: f64,
    pub why: String,
}

fn is_open(status: TodoStatus) -> bool {
    !matches!(status, TodoStatus::Done | TodoStatus::Dropped)
}

/// Orders the queue: items waiting on the user, then by band, then by position (highest
/// first), with creation time as a stable tiebreak. Closed items go last.
#[must_use]
pub fn rank(items: Vec<Item>, now: DateTime<Utc>) -> Vec<Ranked> {
    let mut ranked: Vec<Ranked> = items
        .into_iter()
        .map(|item| {
            let band = band(&item, now);
            let position = position(&item, now);
            Ranked {
                why: String::new(),
                band,
                position,
                item,
            }
        })
        .collect();
    ranked.sort_by(|a, b| {
        let key = |r: &Ranked| {
            (
                !is_open(r.item.status),
                r.item.status != TodoStatus::WaitingOnUser,
                r.band,
            )
        };
        key(a)
            .cmp(&key(b))
            .then(b.position.total_cmp(&a.position))
            .then(a.item.created_at.cmp(&b.item.created_at))
            .then(a.item.id.cmp(&b.item.id))
    });
    for i in 0..ranked.len() {
        let above = i.checked_sub(1).map(|j| ranked[j].item.title.clone());
        let why = explain(&ranked[i], above.as_deref(), now);
        ranked[i].why = why;
    }
    ranked
}

fn explain(r: &Ranked, above: Option<&str>, now: DateTime<Utc>) -> String {
    let item = &r.item;
    match item.status {
        TodoStatus::Done => return "done".to_owned(),
        TodoStatus::Dropped => return "dropped".to_owned(),
        _ => {}
    }
    let mut parts = Vec::new();
    if item.status == TodoStatus::WaitingOnUser {
        parts.push("waiting on you, so it's listed first".to_owned());
    }
    let level = |s: Score| if s.is_high() { "high" } else { "low" };
    let urgency = effective_urgency(item, now);
    let mut band = format!(
        "{:?}: {} importance, {} urgency",
        r.band,
        level(item.importance),
        level(urgency)
    );
    match item.due_at {
        Some(due) if due <= now => band.push_str(" (overdue)"),
        Some(due) if urgency != item.urgency => {
            band.push_str(&format!(" (due in {}h)", (due - now).num_hours().max(0)));
        }
        _ => {}
    }
    parts.push(band);
    if item.manual_rank.is_some() {
        parts.push("placed by you".to_owned());
    } else {
        let mut extra = Vec::new();
        if item.unblocks > 0 {
            extra.push(format!("unblocks {}", item.unblocks));
        }
        if item.status == TodoStatus::Blocked {
            extra.push("blocked".to_owned());
        }
        if !extra.is_empty() {
            parts.push(extra.join(", "));
        }
    }
    if let Some(a) = above {
        parts.push(format!("below \"{a}\""));
    }
    parts.join("; ")
}

/// The manual position for placing an item between two neighbors in the displayed order
/// (positions sort highest first). Either neighbor may be absent at the ends of a band.
#[must_use]
pub fn place_between(above: Option<f64>, below: Option<f64>) -> f64 {
    match (above, below) {
        (Some(a), Some(b)) => (a + b) / 2.0,
        (Some(a), None) => a - 1.0,
        (None, Some(b)) => b + 1.0,
        (None, None) => 50.0,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MoveError {
    #[error("unknown item")]
    Unknown,
    #[error("\"{0}\" is in a different band; change its importance or urgency to move it there")]
    OtherBand(String),
}

/// Moves an item directly above `above_of` or directly below `below_of` within its band.
/// Pins every item in the band to its current position so the whole order you see sticks,
/// and returns the manual positions to store.
pub fn reorder(
    ranked: &[Ranked],
    moved: TodoId,
    above_of: Option<TodoId>,
    below_of: Option<TodoId>,
) -> Result<Vec<(TodoId, f64)>, MoveError> {
    let mover = ranked
        .iter()
        .find(|r| r.item.id == moved)
        .ok_or(MoveError::Unknown)?;
    let band = mover.band;
    let lane: Vec<&Ranked> = ranked
        .iter()
        .filter(|r| {
            r.band == band && is_open(r.item.status) && r.item.status != TodoStatus::WaitingOnUser
        })
        .filter(|r| r.item.id != moved)
        .collect();
    for target in [above_of, below_of].into_iter().flatten() {
        let t = ranked
            .iter()
            .find(|r| r.item.id == target)
            .ok_or(MoveError::Unknown)?;
        if t.band != band {
            return Err(MoveError::OtherBand(t.item.title.clone()));
        }
    }
    let index = match (above_of, below_of) {
        (Some(t), _) => lane.iter().position(|r| r.item.id == t).unwrap_or(0),
        (None, Some(t)) => lane
            .iter()
            .position(|r| r.item.id == t)
            .map_or(lane.len(), |i| i + 1),
        (None, None) => 0,
    };
    // Pin the lane, spacing positions out if any are tied so there is room between them.
    let mut pins: Vec<(TodoId, f64)> = lane.iter().map(|r| (r.item.id, r.position)).collect();
    for i in 1..pins.len() {
        if pins[i].1 >= pins[i - 1].1 {
            pins[i].1 = pins[i - 1].1 - 1.0;
        }
    }
    let above = index.checked_sub(1).map(|i| pins[i].1);
    let below = pins.get(index).map(|p| p.1);
    let mut out: Vec<(TodoId, f64)> = pins
        .into_iter()
        .filter(|(id, p)| {
            lane.iter()
                .find(|r| r.item.id == *id)
                .is_some_and(|r| r.item.manual_rank != Some(*p))
        })
        .collect();
    out.push((moved, place_between(above, below)));
    Ok(out)
}

/// Who is making an edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor {
    User,
    Orchestrator,
}

/// A requested change to one field.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    Title(String),
    Kind(TodoKind),
    Owner(TodoOwner),
    Status(TodoStatus),
    Importance(Score),
    Urgency(Score),
    DueAt(Option<DateTime<Utc>>),
    Workspace(Option<WorkspaceId>),
}

impl Change {
    /// The field name, as recorded in `user_fields` and proposals.
    #[must_use]
    pub fn field(&self) -> &'static str {
        match self {
            Self::Title(_) => "title",
            Self::Kind(_) => "kind",
            Self::Owner(_) => "owner",
            Self::Status(_) => "status",
            Self::Importance(_) => "importance",
            Self::Urgency(_) => "urgency",
            Self::DueAt(_) => "due_at",
            Self::Workspace(_) => "workspace_id",
        }
    }

    /// The new value as JSON, for proposals.
    #[must_use]
    pub fn value(&self) -> serde_json::Value {
        match self {
            Self::Title(v) => serde_json::json!(v),
            Self::Kind(v) => serde_json::json!(v),
            Self::Owner(v) => serde_json::json!(v),
            Self::Status(v) => serde_json::json!(v),
            Self::Importance(v) | Self::Urgency(v) => serde_json::json!(v),
            Self::DueAt(v) => serde_json::json!(v),
            Self::Workspace(v) => serde_json::json!(v),
        }
    }

    /// Parses a proposal's stored field and value back into a change.
    pub fn from_proposal(field: &str, value: &serde_json::Value) -> Option<Self> {
        let v = value.clone();
        Some(match field {
            "title" => Self::Title(serde_json::from_value(v).ok()?),
            "kind" => Self::Kind(serde_json::from_value(v).ok()?),
            "owner" => Self::Owner(serde_json::from_value(v).ok()?),
            "status" => Self::Status(serde_json::from_value(v).ok()?),
            "importance" => Self::Importance(serde_json::from_value(v).ok()?),
            "urgency" => Self::Urgency(serde_json::from_value(v).ok()?),
            "due_at" => Self::DueAt(serde_json::from_value(v).ok()?),
            "workspace_id" => Self::Workspace(serde_json::from_value(v).ok()?),
            _ => return None,
        })
    }

    fn apply(&self, item: &mut Item) {
        match self.clone() {
            Self::Title(v) => item.title = v,
            Self::Kind(v) => item.kind = v,
            Self::Owner(v) => item.owner = v,
            Self::Status(v) => item.status = v,
            Self::Importance(v) => item.importance = v,
            Self::Urgency(v) => item.urgency = v,
            Self::DueAt(v) => item.due_at = v,
            Self::Workspace(v) => item.workspace_id = v,
        }
    }
}

/// Fields the Orchestrator may always change directly: workflow state, not priorities.
const SHARED_FIELDS: &[&str] = &["status", "workspace_id"];

/// The result of applying edits under the ownership rules.
#[derive(Debug, Clone, PartialEq)]
pub struct Edited {
    pub item: Item,
    /// Changes the Orchestrator asked for on fields the user owns; these become proposals.
    pub proposed: Vec<Change>,
}

/// Applies changes. The user's edits apply and mark the field as theirs; the Orchestrator's
/// edits to user-owned fields become proposals instead.
#[must_use]
pub fn edit(mut item: Item, changes: Vec<Change>, actor: Actor) -> Edited {
    let mut proposed = Vec::new();
    for change in changes {
        let field = change.field();
        match actor {
            Actor::User => {
                change.apply(&mut item);
                if !SHARED_FIELDS.contains(&field) && !item.user_fields.iter().any(|f| f == field) {
                    item.user_fields.push(field.to_owned());
                }
            }
            Actor::Orchestrator => {
                if item.user_fields.iter().any(|f| f == field) && !SHARED_FIELDS.contains(&field) {
                    proposed.push(change);
                } else {
                    change.apply(&mut item);
                }
            }
        }
    }
    Edited { item, proposed }
}

/// Applies an accepted proposal as the user's own edit.
#[must_use]
pub fn accept(item: Item, change: Change) -> Item {
    edit(item, vec![change], Actor::User).item
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn s(v: u8) -> Score {
        Score::try_from(v).unwrap()
    }

    fn t0() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }

    fn item(n: u128, title: &str, importance: u8, urgency: u8) -> Item {
        Item {
            id: TodoId::from_ulid(ulid::Ulid(n)),
            title: title.into(),
            kind: TodoKind::Feature,
            owner: TodoOwner::User,
            status: TodoStatus::Ready,
            importance: s(importance),
            urgency: s(urgency),
            due_at: None,
            manual_rank: None,
            workspace_id: None,
            user_fields: vec![],
            created_at: t0(),
            unblocks: 0,
        }
    }

    fn titles(r: &[Ranked]) -> Vec<&str> {
        r.iter().map(|r| r.item.title.as_str()).collect()
    }

    #[test]
    fn bands_order_urgent_before_important() {
        let r = rank(
            vec![
                item(1, "neither", 1, 1),
                item(2, "important", 4, 1),
                item(3, "urgent", 1, 4),
                item(4, "both", 4, 4),
            ],
            t0(),
        );
        assert_eq!(titles(&r), ["both", "urgent", "important", "neither"]);
        assert_eq!(
            r.iter().map(|r| r.band).collect::<Vec<_>>(),
            [Band::P0, Band::P1, Band::P2, Band::P3]
        );
    }

    #[test]
    fn due_soon_raises_urgency_and_overdue_maxes_it() {
        let mut soon = item(1, "soon", 4, 1);
        soon.due_at = Some(t0() + Duration::hours(20));
        assert_eq!(band(&soon, t0()), Band::P0);
        let mut late = item(2, "late", 1, 1);
        late.due_at = Some(t0() - Duration::hours(1));
        assert_eq!(effective_urgency(&late, t0()), Score::MAX);
        let r = rank(vec![soon], t0());
        assert!(r[0].why.contains("due in 20h"), "{}", r[0].why);
    }

    #[test]
    fn waiting_on_user_comes_first_and_says_so() {
        let mut w = item(1, "answer me", 1, 1);
        w.status = TodoStatus::WaitingOnUser;
        let r = rank(vec![item(2, "fire", 4, 4), w], t0());
        assert_eq!(titles(&r), ["answer me", "fire"]);
        assert!(r[0].why.starts_with("waiting on you"));
        assert!(r[1].why.contains("below \"answer me\""));
    }

    fn apply_pins(items: &mut [Item], pins: &[(TodoId, f64)]) {
        for (id, p) in pins {
            if let Some(it) = items.iter_mut().find(|i| i.id == *id) {
                it.manual_rank = Some(*p);
            }
        }
    }

    #[test]
    fn manual_placement_sticks_while_new_items_land_by_score() {
        let mut items = vec![item(1, "a", 3, 1), item(2, "b", 4, 1), item(3, "c", 3, 2)];
        let r = rank(items.clone(), t0());
        assert_eq!(titles(&r), ["b", "c", "a"]);
        // The user moves "a" above "b".
        let pins = reorder(&r, items[0].id, Some(items[1].id), None).unwrap();
        apply_pins(&mut items, &pins);
        let r = rank(items.clone(), t0());
        assert_eq!(titles(&r), ["a", "b", "c"]);
        assert!(r[0].why.contains("placed by you"));
        // Scores change; the order the user saw holds.
        items[2].unblocks = 3;
        items[2].urgency = s(2);
        assert_eq!(titles(&rank(items.clone(), t0())), ["a", "b", "c"]);
        // A new item still lands by its score.
        items.push(item(4, "d", 4, 2));
        assert_eq!(titles(&rank(items.clone(), t0())), ["d", "a", "b", "c"]);
        // Move "c" below "a".
        let r = rank(items.clone(), t0());
        let pins = reorder(&r, items[2].id, None, Some(items[0].id)).unwrap();
        apply_pins(&mut items, &pins);
        assert_eq!(titles(&rank(items.clone(), t0())), ["d", "a", "c", "b"]);
    }

    #[test]
    fn moves_stay_within_a_band() {
        let items = vec![item(1, "low", 1, 1), item(2, "fire", 4, 4)];
        let r = rank(items.clone(), t0());
        assert_eq!(
            reorder(&r, items[0].id, Some(items[1].id), None),
            Err(MoveError::OtherBand("fire".into()))
        );
    }

    #[test]
    fn orchestrator_proposes_on_user_owned_fields() {
        let mut it = item(1, "x", 2, 2);
        it = edit(it, vec![Change::Importance(s(4))], Actor::User).item;
        assert_eq!(it.user_fields, ["importance"]);
        let e = edit(
            it,
            vec![
                Change::Importance(s(1)),
                Change::Urgency(s(4)),
                Change::Status(TodoStatus::Active),
            ],
            Actor::Orchestrator,
        );
        assert_eq!(e.item.importance, s(4), "user value kept");
        assert_eq!(e.item.urgency, s(4), "unowned field applied");
        assert_eq!(e.item.status, TodoStatus::Active, "status is shared");
        assert_eq!(e.proposed, vec![Change::Importance(s(1))]);
        let accepted = accept(e.item, e.proposed[0].clone());
        assert_eq!(accepted.importance, s(1));
    }

    #[test]
    fn proposal_values_round_trip() {
        for c in [
            Change::Title("t".into()),
            Change::Kind(TodoKind::Review),
            Change::Owner(TodoOwner::Orchestrator),
            Change::Status(TodoStatus::Blocked),
            Change::Importance(s(3)),
            Change::Urgency(s(1)),
            Change::DueAt(Some(t0())),
            Change::DueAt(None),
            Change::Workspace(None),
        ] {
            assert_eq!(Change::from_proposal(c.field(), &c.value()), Some(c));
        }
    }

    fn arb_item() -> impl Strategy<Value = Item> {
        (
            any::<u64>(),
            1u8..=4,
            1u8..=4,
            prop::option::of(-100i64..200),
            prop::option::of(0.0f64..100.0),
            0u32..5,
            prop_oneof![
                Just(TodoStatus::Ready),
                Just(TodoStatus::Blocked),
                Just(TodoStatus::WaitingOnUser),
                Just(TodoStatus::Done),
                Just(TodoStatus::Active)
            ],
            0i64..400,
        )
            .prop_map(|(n, i, u, due, manual, unblocks, status, age)| {
                let mut it = item(u128::from(n), &format!("t{n}"), i, u);
                it.due_at = due.map(|h| t0() + Duration::hours(h));
                it.manual_rank = manual;
                it.unblocks = unblocks;
                it.status = status;
                it.created_at = t0() - Duration::hours(age);
                it
            })
    }

    proptest! {
        #[test]
        fn ranking_is_a_stable_total_order(items in prop::collection::vec(arb_item(), 0..30)) {
            let a = rank(items.clone(), t0());
            let mut reversed = items.clone();
            reversed.reverse();
            let b = rank(reversed, t0());
            let ids = |r: &[Ranked]| r.iter().map(|r| r.item.id).collect::<Vec<_>>();
            prop_assert_eq!(ids(&a), ids(&b), "input order doesn't matter");
            prop_assert_eq!(a.len(), items.len());
            for w in a.windows(2) {
                let (x, y) = (&w[0], &w[1]);
                if is_open(x.item.status) && is_open(y.item.status)
                    && (x.item.status == TodoStatus::WaitingOnUser) == (y.item.status == TodoStatus::WaitingOnUser)
                {
                    prop_assert!(x.band <= y.band);
                    if x.band == y.band {
                        prop_assert!(x.position >= y.position);
                    }
                }
                prop_assert!(!x.why.is_empty());
            }
        }

        #[test]
        fn orchestrator_never_writes_user_owned_priorities(it in arb_item(), imp in 1u8..=4, urg in 1u8..=4) {
            let owned = edit(it, vec![Change::Importance(s(imp)), Change::Urgency(s(urg))], Actor::User).item;
            let e = edit(owned.clone(), vec![Change::Importance(s(5 - imp)), Change::Urgency(s(5 - urg))], Actor::Orchestrator);
            prop_assert_eq!(e.item.importance, owned.importance);
            prop_assert_eq!(e.item.urgency, owned.urgency);
            prop_assert_eq!(e.proposed.len(), 2);
        }

        #[test]
        fn placing_between_neighbors_lands_between(a in 0.0f64..100.0, b in 0.0f64..100.0) {
            let (hi, lo) = if a >= b { (a, b) } else { (b, a) };
            let p = place_between(Some(hi), Some(lo));
            prop_assert!(p <= hi && p >= lo);
        }
    }
}
