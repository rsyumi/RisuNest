use risunest_sync_wire::{RecordChange, RecordVersion};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Decision {
    Unchanged,
    AcceptRemote,
    PublishLocal,
    Identical,
    Conflict,
}
fn same_result(a: &RecordVersion, b: &RecordVersion) -> bool {
    a == b
        || matches!(
            (a, b),
            (
                RecordVersion::Tombstone { .. },
                RecordVersion::Tombstone { .. }
            )
        )
}
pub(crate) fn decide(
    base: &RecordVersion,
    local: &RecordVersion,
    remote: &RecordVersion,
) -> Decision {
    if local == base && remote == base {
        Decision::Unchanged
    } else if same_result(local, remote) {
        Decision::Identical
    } else if local == base {
        Decision::AcceptRemote
    } else if remote == base {
        Decision::PublishLocal
    } else {
        Decision::Conflict
    }
}
pub(crate) fn proposal(
    key: String,
    base: RecordVersion,
    local: RecordVersion,
    remote: &RecordVersion,
) -> Option<RecordChange> {
    (decide(&base, &local, remote) == Decision::PublishLocal).then_some(RecordChange {
        key,
        before: remote.clone(),
        after: local,
    })
}
pub(crate) fn clear_conflicts(original_scope: Option<&str>, remote_scope: &str) -> bool {
    original_scope != Some(remote_scope)
}
pub(crate) fn read_dependency_conflicts(
    observed: &RecordVersion,
    remote: &RecordVersion,
    has_local_change: bool,
) -> bool {
    has_local_change && observed != remote
}

#[cfg(test)]
mod tests {
    use super::*;
    fn live(value: &[u8]) -> RecordVersion {
        RecordVersion::Live {
            object_hash: risunest_sync_wire::hash(value),
            descriptor_hash: None,
        }
    }
    #[test]
    fn independent_same_key_delete_set_and_identical_outcomes_preserve_intent() {
        let base = live(b"base");
        let a = live(b"a");
        let b = live(b"b");
        let absent = RecordVersion::Absent;
        for (base, local, remote, expected) in [
            (&base, &a, &base, Decision::PublishLocal),
            (&base, &base, &b, Decision::AcceptRemote),
            (&base, &a, &b, Decision::Conflict),
            (&base, &a, &a, Decision::Identical),
            (&absent, &a, &b, Decision::Conflict),
            (&absent, &absent, &b, Decision::AcceptRemote),
            (&base, &base, &base, Decision::Unchanged),
        ] {
            assert_eq!(decide(base, local, remote), expected);
        }
        let deletion = RecordVersion::Tombstone {
            deletion_id: "a".into(),
        };
        assert_eq!(decide(&base, &deletion, &b), Decision::Conflict);
        assert_eq!(
            decide(
                &base,
                &deletion,
                &RecordVersion::Tombstone {
                    deletion_id: "b".into()
                }
            ),
            Decision::Identical
        );
        assert!(proposal("key".into(), base.clone(), a.clone(), &b).is_none());
        assert_eq!(
            proposal("key".into(), base.clone(), a.clone(), &base)
                .unwrap()
                .before,
            base
        );
    }
    #[test]
    fn clear_and_parent_changes_cannot_be_silently_rebased() {
        assert!(clear_conflicts(Some("original"), "new-key-or-clear"));
        assert!(clear_conflicts(None, "current"));
        assert!(!clear_conflicts(Some("same"), "same"));
        assert!(read_dependency_conflicts(
            &live(b"parent"),
            &RecordVersion::Tombstone {
                deletion_id: "deleted".into()
            },
            true
        ));
        assert!(!read_dependency_conflicts(
            &live(b"parent"),
            &live(b"parent"),
            true
        ));
    }
}
