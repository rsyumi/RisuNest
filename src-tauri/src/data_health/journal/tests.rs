use super::*;
use crate::data_health::repair::{RepairAction, RepairCandidate};

fn journal(id: &str, created_at: i64, objects: &[&str]) -> Journal {
    Journal {
        id: id.to_owned(),
        created_at,
        from_revision: 1,
        to_revision: 2,
        applied: vec![RepairCandidate {
            id: "0:drop-alias".to_owned(),
            action: RepairAction::DropAlias {
                kind: "asset".to_owned(),
                key: "assets/gone.png".to_owned(),
            },
            finding: 0,
            preferred: true,
            discards: true,
        }],
        records: Vec::new(),
        released_objects: objects.iter().map(|hash| (*hash).to_owned()).collect(),
    }
}

#[test]
fn a_journal_round_trips_and_lists_newest_first() {
    let directory = tempfile::tempdir().unwrap();
    for index in 0..3 {
        write(directory.path(), &journal(&format!("repair-{index}"), index, &[])).unwrap();
    }
    let listed = list(directory.path()).unwrap();
    assert_eq!(
        listed.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>(),
        ["repair-2", "repair-1", "repair-0"]
    );
    assert_eq!(
        read(directory.path(), "repair-1").unwrap().unwrap(),
        journal("repair-1", 1, &[])
    );
    assert!(read(directory.path(), "repair-9").unwrap().is_none());
}

#[test]
fn a_live_journal_holds_the_objects_a_repair_stopped_referencing() {
    let directory = tempfile::tempdir().unwrap();
    let hash = "ab".repeat(32);
    write(directory.path(), &journal("repair-0", 0, &[&hash])).unwrap();
    assert!(roots(directory.path()).unwrap().object_hashes.contains(&hash));

    remove(directory.path(), "repair-0").unwrap();
    assert!(
        roots(directory.path()).unwrap().object_hashes.is_empty(),
        "what no journal holds returns to the cleanup's candidates"
    );
}

#[test]
fn rotation_drops_the_oldest_once_the_kept_count_is_reached() {
    let directory = tempfile::tempdir().unwrap();
    let hash = "cd".repeat(32);
    for index in 0..(KEPT as i64 + 2) {
        let held: Vec<&str> = match index {
            0 => vec![hash.as_str()],
            _ => Vec::new(),
        };
        write(
            directory.path(),
            &journal(&format!("repair-{index}"), index, &held),
        )
        .unwrap();
        // The newest file must also be the most recently modified for rotation to see the order.
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let listed = list(directory.path()).unwrap();
    assert_eq!(listed.len(), KEPT);
    assert!(!listed.iter().any(|entry| entry.id == "repair-0"));
    assert!(
        !roots(directory.path()).unwrap().object_hashes.contains(&hash),
        "a rotated journal stops holding what it held"
    );
}

#[test]
fn a_file_the_build_cannot_read_is_not_a_journal_and_never_fails_the_list() {
    let directory = tempfile::tempdir().unwrap();
    write(directory.path(), &journal("repair-0", 0, &[])).unwrap();
    std::fs::create_dir_all(super::directory(directory.path())).unwrap();
    std::fs::write(
        super::directory(directory.path()).join("repair-broken.json"),
        b"{ not a journal",
    )
    .unwrap();
    assert_eq!(list(directory.path()).unwrap().len(), 1);
}
