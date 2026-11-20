//! Per-item decisions: write, skip as unchanged, or record a conflict
//! (design §9.2 "Idempotency" and "Conflicts").

use chrono::{DateTime, Utc};
use helios_endpoints::sync::{ConflictPolicy, SyncItemState};

/// What the target currently holds for one resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetInfo {
    /// `meta.versionId` at the target.
    pub version: Option<String>,
    /// `meta.lastUpdated` at the target.
    pub last_updated: Option<DateTime<Utc>>,
    /// Canonical content hash at the target.
    pub hash: [u8; 32],
}

/// The decision for one change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Write (or delete) at the target. `if_match` = the target version this
    /// job last wrote, for servers that support conditional writes.
    Apply {
        /// Precondition version, when the policy wants one.
        if_match: Option<String>,
    },
    /// Nothing to do: the target already has this state.
    Skip,
    /// Do not write; record a conflict with this reason.
    Conflict(String),
}

/// Whether the decision needs the target's current state.
pub fn needs_target(policy: ConflictPolicy, deleted: bool) -> bool {
    !matches!(policy, ConflictPolicy::SourceWins) || deleted
}

/// The item state already says the target has exactly this change (fast
/// path, no target read).
pub fn unchanged_since_last_apply(
    state: Option<&SyncItemState>,
    deleted: bool,
    hash: Option<&[u8; 32]>,
) -> bool {
    match state {
        None => false,
        Some(s) if deleted => s.deleted,
        Some(s) => !s.deleted && s.content_sha256.as_deref() == hash.map(|h| h.as_slice()),
    }
}

/// Decides one change.
///
/// * `source-wins`: apply unless the item state shows the same content was
///   already applied (no target read).
/// * `skip-if-target-newer`: conflict when the target's `meta.lastUpdated`
///   is newer than the source change.
/// * `fail`: conflict when the target changed since this job last wrote it
///   (its version differs from the recorded one, or — for a resource this
///   job never wrote — the target already holds different content).
///
/// Deletes of a resource the target does not have are skipped.
pub fn decide(
    policy: ConflictPolicy,
    state: Option<&SyncItemState>,
    deleted: bool,
    source_hash: Option<&[u8; 32]>,
    source_time: DateTime<Utc>,
    target: Option<&TargetInfo>,
) -> Decision {
    if unchanged_since_last_apply(state, deleted, source_hash) {
        let target_untouched = match (policy, target, state) {
            (ConflictPolicy::SourceWins, _, _) => true,
            (_, None, _) => deleted,
            (_, Some(t), Some(s)) => s.target_version.is_none() || t.version == s.target_version,
            (_, Some(_), None) => false,
        };
        if target_untouched {
            return Decision::Skip;
        }
    }
    if deleted && needs_target(policy, deleted) && target.is_none() {
        return Decision::Skip;
    }
    if let (Some(t), Some(h)) = (target, source_hash)
        && !deleted
        && &t.hash == h
    {
        // Same content already there (e.g. after a storage copy).
        return Decision::Skip;
    }
    match policy {
        ConflictPolicy::SourceWins => Decision::Apply { if_match: None },
        ConflictPolicy::SkipIfTargetNewer => match target.and_then(|t| t.last_updated) {
            Some(t) if t > source_time => Decision::Conflict(format!(
                "target is newer ({}) than the source change ({})",
                t.to_rfc3339(),
                source_time.to_rfc3339()
            )),
            _ => Decision::Apply { if_match: None },
        },
        ConflictPolicy::Fail => match (target, state) {
            (None, _) => Decision::Apply { if_match: None },
            (Some(t), Some(s)) if s.target_version.is_some() => {
                if t.version == s.target_version {
                    Decision::Apply {
                        if_match: s.target_version.clone(),
                    }
                } else {
                    Decision::Conflict(format!(
                        "target changed since this job wrote it (version {} -> {})",
                        s.target_version.as_deref().unwrap_or("?"),
                        t.version.as_deref().unwrap_or("?")
                    ))
                }
            }
            (Some(_), _) => Decision::Conflict(
                "target holds a different resource this job did not write".into(),
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn h(b: u8) -> [u8; 32] {
        [b; 32]
    }

    fn state(hash: u8, target_version: Option<&str>, deleted: bool) -> SyncItemState {
        SyncItemState {
            resource_type: "Patient".into(),
            resource_id: "p".into(),
            source_version: Some("1".into()),
            target_version: target_version.map(str::to_string),
            content_sha256: Some(h(hash).to_vec()),
            deleted,
            applied_at: Utc::now(),
        }
    }

    fn target(v: &str, hash: u8, at: DateTime<Utc>) -> TargetInfo {
        TargetInfo {
            version: Some(v.into()),
            last_updated: Some(at),
            hash: h(hash),
        }
    }

    #[test]
    fn source_wins_skips_unchanged_without_target() {
        let now = Utc::now();
        let s = state(1, Some("4"), false);
        assert_eq!(
            decide(
                ConflictPolicy::SourceWins,
                Some(&s),
                false,
                Some(&h(1)),
                now,
                None
            ),
            Decision::Skip
        );
        assert_eq!(
            decide(
                ConflictPolicy::SourceWins,
                Some(&s),
                false,
                Some(&h(2)),
                now,
                None
            ),
            Decision::Apply { if_match: None }
        );
        assert_eq!(
            decide(
                ConflictPolicy::SourceWins,
                None,
                false,
                Some(&h(2)),
                now,
                None
            ),
            Decision::Apply { if_match: None }
        );
        assert!(!needs_target(ConflictPolicy::SourceWins, false));
        assert!(needs_target(ConflictPolicy::SourceWins, true));
        assert!(needs_target(ConflictPolicy::Fail, false));
    }

    #[test]
    fn deletes() {
        let now = Utc::now();
        let gone = state(1, None, true);
        assert_eq!(
            decide(
                ConflictPolicy::SourceWins,
                Some(&gone),
                true,
                None,
                now,
                None
            ),
            Decision::Skip
        );
        // Target does not have it: nothing to delete.
        assert_eq!(
            decide(ConflictPolicy::SourceWins, None, true, None, now, None),
            Decision::Skip
        );
        let t = target("2", 9, now);
        assert_eq!(
            decide(ConflictPolicy::SourceWins, None, true, None, now, Some(&t)),
            Decision::Apply { if_match: None }
        );
    }

    #[test]
    fn same_content_at_target_is_skipped() {
        let now = Utc::now();
        let t = target("1", 5, now);
        for p in [
            ConflictPolicy::SourceWins,
            ConflictPolicy::SkipIfTargetNewer,
            ConflictPolicy::Fail,
        ] {
            assert_eq!(
                decide(p, None, false, Some(&h(5)), now, Some(&t)),
                Decision::Skip
            );
        }
    }

    #[test]
    fn skip_if_target_newer() {
        let now = Utc::now();
        let newer = target("3", 9, now + Duration::seconds(5));
        let older = target("3", 9, now - Duration::seconds(5));
        assert!(matches!(
            decide(
                ConflictPolicy::SkipIfTargetNewer,
                None,
                false,
                Some(&h(1)),
                now,
                Some(&newer)
            ),
            Decision::Conflict(_)
        ));
        assert_eq!(
            decide(
                ConflictPolicy::SkipIfTargetNewer,
                None,
                false,
                Some(&h(1)),
                now,
                Some(&older)
            ),
            Decision::Apply { if_match: None }
        );
        assert_eq!(
            decide(
                ConflictPolicy::SkipIfTargetNewer,
                None,
                false,
                Some(&h(1)),
                now,
                None
            ),
            Decision::Apply { if_match: None }
        );
    }

    #[test]
    fn fail_policy_detects_target_edits() {
        let now = Utc::now();
        let s = state(1, Some("4"), false);
        // Target still at the version we wrote: apply with If-Match.
        let same = target("4", 1, now);
        assert_eq!(
            decide(
                ConflictPolicy::Fail,
                Some(&s),
                false,
                Some(&h(2)),
                now,
                Some(&same)
            ),
            Decision::Apply {
                if_match: Some("4".into())
            }
        );
        // Someone edited the target.
        let edited = target("5", 7, now);
        assert!(matches!(
            decide(ConflictPolicy::Fail, Some(&s), false, Some(&h(2)), now, Some(&edited)),
            Decision::Conflict(r) if r.contains("4 -> 5")
        ));
        // Unchanged source but edited target: not a silent skip.
        assert!(matches!(
            decide(
                ConflictPolicy::Fail,
                Some(&s),
                false,
                Some(&h(1)),
                now,
                Some(&edited)
            ),
            Decision::Conflict(_)
        ));
        // Unchanged source and untouched target: skip.
        assert_eq!(
            decide(
                ConflictPolicy::Fail,
                Some(&s),
                false,
                Some(&h(1)),
                now,
                Some(&same)
            ),
            Decision::Skip
        );
        // A resource the job never wrote, different content at the target.
        assert!(matches!(
            decide(
                ConflictPolicy::Fail,
                None,
                false,
                Some(&h(2)),
                now,
                Some(&edited)
            ),
            Decision::Conflict(_)
        ));
        // Target missing: create.
        assert_eq!(
            decide(
                ConflictPolicy::Fail,
                Some(&s),
                false,
                Some(&h(2)),
                now,
                None
            ),
            Decision::Apply { if_match: None }
        );
    }

    #[test]
    fn fast_path_predicate() {
        assert!(!unchanged_since_last_apply(None, false, Some(&h(1))));
        let live = state(1, None, false);
        assert!(unchanged_since_last_apply(Some(&live), false, Some(&h(1))));
        assert!(!unchanged_since_last_apply(Some(&live), true, None));
        let gone = state(1, None, true);
        assert!(!unchanged_since_last_apply(Some(&gone), false, Some(&h(1))));
    }
}
