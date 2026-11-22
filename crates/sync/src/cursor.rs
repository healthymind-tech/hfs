//! Run planning and checkpoint arithmetic (design §9.2 "Incremental").
//!
//! A run's position is a [`SyncCursor`]: the phase (`initial` load or
//! `incremental` polling), the initial load's remaining types and source
//! page, and the incremental **high-water mark** with its **boundary**.
//!
//! * Incremental reads ask the source for changes at or after the
//!   high-water mark (`_history?_since=hw` is inclusive), so changes that
//!   share the newest applied timestamp are never lost; the boundary lists
//!   the `type/id/version` tokens already applied at exactly that instant so
//!   they are not applied twice.
//! * The high-water mark only advances to `now - safety lag`: a writer whose
//!   transaction started earlier may still commit a change stamped before
//!   "now". Changes newer than that are still applied (idempotent) and simply
//!   re-read by the next pass, where the item state recognises them.
//! * Each run starts from the newest previous run's cursor, so a failed,
//!   cancelled or crashed run resumes where it stopped.

use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use helios_endpoints::sync::{InitialLoad, SyncCursor, SyncJobSpec};

use crate::change::Change;

/// Phase name of the initial load.
pub const PHASE_INITIAL: &str = "initial";
/// Phase name of incremental polling.
pub const PHASE_INCREMENTAL: &str = "incremental";
/// The "every type" marker a source that reads all types in one ordered
/// stream (a local endpoint) plans as its single initial-load type.
pub const ALL_TYPES: &str = "*";

/// The cursor a new run starts from: the newest previous run's cursor when
/// it has one, else a fresh plan for the job. Without an initial load the
/// first incremental pass starts at the job's `since`, else at `created`
/// (when the job was created: changes made after that are not lost to a
/// queued run starting late).
pub fn start_cursor(
    spec: &SyncJobSpec,
    previous: Option<&SyncCursor>,
    now: DateTime<Utc>,
    created: DateTime<Utc>,
    lag: Duration,
) -> SyncCursor {
    if let Some(p) = previous.filter(|p| !p.phase.is_empty()) {
        return p.clone();
    }
    match spec.initial_load {
        InitialLoad::None => SyncCursor {
            phase: PHASE_INCREMENTAL.into(),
            high_water: Some(spec.since.unwrap_or(created)),
            ..SyncCursor::default()
        },
        _ => SyncCursor {
            phase: PHASE_INITIAL.into(),
            // Changes made while the initial load runs are picked up by the
            // first incremental pass (and skipped when already applied).
            high_water: Some(now - lag),
            ..SyncCursor::default()
        },
    }
}

/// Whether the initial load has not been planned yet.
pub fn needs_plan(c: &SyncCursor) -> bool {
    c.phase == PHASE_INITIAL && c.current_type.is_none() && c.pending_types.is_empty()
}

/// The initial-load types: the job's explicit list, else every type the
/// source offers that is in the job's scope (sorted, deduplicated).
pub fn plan_types(spec: &SyncJobSpec, available: &[String]) -> Vec<String> {
    // A source that loads through one bulk export plans exactly that.
    if let [only] = available
        && only.starts_with('$')
    {
        return vec![only.clone()];
    }
    let mut out: Vec<String> = Vec::new();
    let candidates: Vec<&String> = if spec.resource_types.is_empty() {
        let mut v: Vec<&String> = available
            .iter()
            .filter(|t| t.as_str() == ALL_TYPES || spec.includes_type(t))
            .collect();
        v.sort();
        v
    } else {
        spec.resource_types.iter().collect()
    };
    for t in candidates {
        if !out.contains(t) {
            out.push(t.clone());
        }
    }
    out
}

/// Installs a plan; with nothing to load the cursor moves to incremental.
pub fn install_plan(c: &mut SyncCursor, mut types: Vec<String>) {
    if types.is_empty() {
        finish_initial(c);
        return;
    }
    c.current_type = Some(types.remove(0));
    c.pending_types = types;
    c.page = None;
}

/// The current type finished: moves to the next, or ends the initial load.
pub fn next_type(c: &mut SyncCursor) {
    c.page = None;
    if c.pending_types.is_empty() {
        finish_initial(c);
    } else {
        c.current_type = Some(c.pending_types.remove(0));
    }
}

fn finish_initial(c: &mut SyncCursor) {
    c.phase = PHASE_INCREMENTAL.into();
    c.current_type = None;
    c.pending_types.clear();
    c.page = None;
}

/// Drops changes the cursor already covers: older than the high-water mark,
/// or at exactly the mark and listed in the boundary.
pub fn admit(c: &SyncCursor, changes: Vec<Change>) -> Vec<Change> {
    let Some(hw) = c.high_water else {
        return changes;
    };
    let boundary: HashSet<&str> = c.boundary.iter().map(String::as_str).collect();
    changes
        .into_iter()
        .filter(|ch| {
            ch.last_updated > hw
                || (ch.last_updated == hw && !boundary.contains(ch.token().as_str()))
        })
        .collect()
}

fn version_number(v: &Option<String>) -> i64 {
    v.as_deref().and_then(|s| s.parse().ok()).unwrap_or(-1)
}

/// Sorts changes oldest first and keeps only the newest change of each
/// resource (a mirror needs the final state; intermediate versions of the
/// same window are not replayed).
pub fn order_and_dedupe(mut changes: Vec<Change>) -> Vec<Change> {
    changes.sort_by(|a, b| {
        a.last_updated
            .cmp(&b.last_updated)
            .then_with(|| version_number(&a.version).cmp(&version_number(&b.version)))
            .then_with(|| a.deleted.cmp(&b.deleted))
    });
    let mut last: BTreeMap<String, usize> = BTreeMap::new();
    for (i, c) in changes.iter().enumerate() {
        last.insert(c.key(), i);
    }
    let keep: HashSet<usize> = last.into_values().collect();
    changes
        .into_iter()
        .enumerate()
        .filter(|(i, _)| keep.contains(i))
        .map(|(_, c)| c)
        .collect()
}

/// Advances the high-water mark over a batch that was applied (oldest
/// first), never past `limit` (= the source window's completeness bound and
/// `now - safety lag`, whichever is earlier).
pub fn advance(c: &mut SyncCursor, applied: &[Change], limit: DateTime<Utc>) {
    let Some(max_ts) = applied
        .iter()
        .map(|ch| ch.last_updated)
        .filter(|t| *t <= limit)
        .max()
    else {
        return;
    };
    let at_max = applied
        .iter()
        .filter(|ch| ch.last_updated == max_ts)
        .map(Change::token);
    match c.high_water {
        Some(hw) if max_ts < hw => {}
        Some(hw) if max_ts == hw => {
            for t in at_max {
                if !c.boundary.contains(&t) {
                    c.boundary.push(t);
                }
            }
        }
        _ => {
            c.high_water = Some(max_ts);
            c.boundary = at_max.collect();
            c.boundary.dedup();
        }
    }
}

/// Splits ordered changes into batches of `size` (≥ 1).
pub fn batches(changes: Vec<Change>, size: usize) -> Vec<Vec<Change>> {
    let size = size.max(1);
    let mut out = Vec::new();
    let mut it = changes.into_iter().peekable();
    while it.peek().is_some() {
        out.push(it.by_ref().take(size).collect());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use helios_endpoints::sync::{SyncPeer, SyncTrigger};
    use serde_json::json;

    fn ts(s: u32) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            + Duration::seconds(s as i64)
    }

    fn ch(t: &str, id: &str, v: &str, at: u32) -> Change {
        Change {
            resource_type: t.into(),
            id: id.into(),
            version: Some(v.into()),
            last_updated: ts(at),
            deleted: false,
            resource: Some(json!({"resourceType": t, "id": id})),
        }
    }

    fn spec(initial: InitialLoad) -> SyncJobSpec {
        SyncJobSpec {
            name: "j".into(),
            description: None,
            source: SyncPeer::Endpoint { name: "a".into() },
            target: SyncPeer::Endpoint { name: "b".into() },
            resource_types: vec![],
            filters: Default::default(),
            since: None,
            initial_load: initial,
            include_history: false,
            trigger: SyncTrigger::Manual,
            conflict: Default::default(),
            propagate_deletes: true,
            id_strategy: Default::default(),
            batch_size: 2,
            enabled: true,
        }
    }

    #[test]
    fn start_cursor_plans_or_resumes() {
        let now = ts(100);
        let lag = Duration::seconds(2);
        let c = start_cursor(&spec(InitialLoad::PagedRead), None, now, now, lag);
        assert_eq!(c.phase, PHASE_INITIAL);
        assert_eq!(c.high_water, Some(ts(98)));
        assert!(needs_plan(&c));

        let mut s = spec(InitialLoad::None);
        let c = start_cursor(&s, None, now, ts(40), lag);
        assert_eq!(c.phase, PHASE_INCREMENTAL);
        assert_eq!(c.high_water, Some(ts(40)));
        s.since = Some(ts(5));
        assert_eq!(
            start_cursor(&s, None, now, ts(40), lag).high_water,
            Some(ts(5))
        );

        let prev = SyncCursor {
            phase: PHASE_INCREMENTAL.into(),
            high_water: Some(ts(50)),
            boundary: vec!["Patient/1/2".into()],
            ..Default::default()
        };
        assert_eq!(start_cursor(&s, Some(&prev), now, now, lag), prev);
        // An empty previous cursor (a run that never started) is a fresh plan.
        assert_eq!(
            start_cursor(&s, Some(&SyncCursor::default()), now, now, lag).phase,
            PHASE_INCREMENTAL
        );
    }

    #[test]
    fn type_plan_walk() {
        let mut s = spec(InitialLoad::PagedRead);
        let avail = vec![
            "Observation".to_string(),
            "AuditEvent".to_string(),
            "Patient".to_string(),
            "Patient".to_string(),
        ];
        assert_eq!(plan_types(&s, &avail), vec!["Observation", "Patient"]);
        assert_eq!(plan_types(&s, &[ALL_TYPES.to_string()]), vec!["*"]);
        let mut b = s.clone();
        b.resource_types = vec!["Patient".into()];
        assert_eq!(plan_types(&b, &["$export".to_string()]), vec!["$export"]);
        s.resource_types = vec!["Patient".into(), "AuditEvent".into(), "Patient".into()];
        assert_eq!(plan_types(&s, &avail), vec!["Patient", "AuditEvent"]);

        let mut c = start_cursor(&s, None, ts(10), ts(10), Duration::zero());
        install_plan(&mut c, vec!["A".into(), "B".into()]);
        assert_eq!(c.current_type.as_deref(), Some("A"));
        c.page = Some("p2".into());
        next_type(&mut c);
        assert_eq!(c.current_type.as_deref(), Some("B"));
        assert!(c.page.is_none());
        next_type(&mut c);
        assert_eq!(c.phase, PHASE_INCREMENTAL);
        assert!(c.current_type.is_none());

        let mut empty = start_cursor(&s, None, ts(10), ts(10), Duration::zero());
        install_plan(&mut empty, vec![]);
        assert_eq!(empty.phase, PHASE_INCREMENTAL);
        assert!(!needs_plan(&empty));
    }

    #[test]
    fn admit_skips_covered_changes() {
        let c = SyncCursor {
            phase: PHASE_INCREMENTAL.into(),
            high_water: Some(ts(10)),
            boundary: vec!["Patient/a/1".into()],
            ..Default::default()
        };
        let got = admit(
            &c,
            vec![
                ch("Patient", "old", "1", 9),
                ch("Patient", "a", "1", 10),
                ch("Patient", "b", "1", 10),
                ch("Patient", "a", "2", 10),
                ch("Patient", "c", "1", 11),
            ],
        );
        let tokens: Vec<String> = got.iter().map(Change::token).collect();
        assert_eq!(tokens, vec!["Patient/b/1", "Patient/a/2", "Patient/c/1"]);
        let no_hw = SyncCursor::default();
        assert_eq!(admit(&no_hw, vec![ch("A", "1", "1", 1)]).len(), 1);
    }

    #[test]
    fn dedupe_keeps_newest_per_resource_in_time_order() {
        let mut del = Change::delete("Patient", "a", None, ts(7));
        del.version = Some("3".into());
        let got = order_and_dedupe(vec![
            ch("Patient", "a", "2", 5),
            ch("Observation", "o", "1", 6),
            ch("Patient", "a", "1", 3),
            del,
            ch("Patient", "b", "1", 1),
        ]);
        let tokens: Vec<String> = got.iter().map(Change::token).collect();
        assert_eq!(
            tokens,
            vec!["Patient/b/1", "Observation/o/1", "Patient/a/3"]
        );
        assert!(got[2].deleted);
        // Same instant: the higher version wins.
        let got = order_and_dedupe(vec![ch("P", "x", "2", 5), ch("P", "x", "1", 5)]);
        assert_eq!(got[0].version.as_deref(), Some("2"));
    }

    #[test]
    fn advance_respects_limit_and_boundary() {
        let mut c = SyncCursor {
            phase: PHASE_INCREMENTAL.into(),
            high_water: Some(ts(10)),
            ..Default::default()
        };
        // Batch past the limit: advance only to the newest change ≤ limit.
        advance(
            &mut c,
            &[
                ch("P", "a", "1", 11),
                ch("P", "b", "1", 12),
                ch("P", "c", "1", 20),
            ],
            ts(15),
        );
        assert_eq!(c.high_water, Some(ts(12)));
        assert_eq!(c.boundary, vec!["P/b/1"]);
        // Another change at the same instant extends the boundary.
        advance(&mut c, &[ch("P", "d", "1", 12)], ts(15));
        assert_eq!(c.boundary, vec!["P/b/1", "P/d/1"]);
        // Nothing eligible: unchanged.
        let before = c.clone();
        advance(&mut c, &[ch("P", "z", "1", 30)], ts(15));
        assert_eq!(c, before);
        // Older than the mark: unchanged.
        advance(&mut c, &[ch("P", "y", "1", 5)], ts(15));
        assert_eq!(c, before);
        // A fresh cursor without a mark takes the batch's newest instant.
        let mut f = SyncCursor::default();
        advance(&mut f, &[ch("P", "a", "1", 3), ch("P", "b", "1", 3)], ts(9));
        assert_eq!(f.high_water, Some(ts(3)));
        assert_eq!(f.boundary.len(), 2);
    }

    #[test]
    fn resume_after_partial_window_applies_each_change_once() {
        // Simulate a crash after the first batch: the second pass re-reads
        // from the checkpoint and only admits what was not applied.
        let window = vec![
            ch("P", "a", "1", 1),
            ch("P", "b", "1", 2),
            ch("P", "c", "1", 2),
            ch("P", "d", "1", 3),
        ];
        let mut c = SyncCursor {
            phase: PHASE_INCREMENTAL.into(),
            high_water: Some(ts(0)),
            ..Default::default()
        };
        let mut applied: Vec<String> = Vec::new();
        let first = batches(order_and_dedupe(admit(&c, window.clone())), 2)
            .into_iter()
            .next()
            .unwrap();
        applied.extend(first.iter().map(Change::token));
        advance(&mut c, &first, ts(100));
        // "Crash" — re-read the whole window from the checkpoint.
        for b in batches(order_and_dedupe(admit(&c, window)), 2) {
            applied.extend(b.iter().map(Change::token));
            advance(&mut c, &b, ts(100));
        }
        assert_eq!(applied, vec!["P/a/1", "P/b/1", "P/c/1", "P/d/1"]);
        assert_eq!(c.high_water, Some(ts(3)));
    }

    #[test]
    fn batching() {
        let v: Vec<Change> = (0..5).map(|i| ch("P", &i.to_string(), "1", i)).collect();
        let b = batches(v, 2);
        assert_eq!(b.iter().map(Vec::len).collect::<Vec<_>>(), vec![2, 2, 1]);
        assert!(batches(vec![], 3).is_empty());
        assert_eq!(batches(vec![ch("P", "x", "1", 0)], 0).len(), 1);
    }
}
