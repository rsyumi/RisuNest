use super::*;
use crate::data_health::{Finding, FindingSink, Findings, ScanDepth, ScanResult, Severity};

fn result(items: Vec<Finding>) -> ScanResult {
    let mut findings = Findings::new(64);
    for item in items {
        findings.note(item);
    }
    ScanResult::new(7, 1, ScanDepth::Quick, findings)
}

fn reference() -> Finding {
    Finding::new(
        codes::REFERENCE_MISSING,
        "character",
        "char-1",
        "reference has no target in this library",
    )
    .at("$.image", 0)
    .targeting("asset", "assets/portrait.png")
}

#[test]
fn a_dangling_reference_offers_removing_it_and_says_the_value_is_gone() {
    let plan = plan(&result(vec![reference()]));
    let [candidate] = plan.as_slice() else {
        panic!("one candidate: {plan:?}");
    };
    assert!(candidate.preferred && candidate.discards);
    assert_eq!(
        candidate.action,
        RepairAction::DropReference {
            owner: Owner {
                kind: "character".to_owned(),
                id: "char-1".to_owned(),
            },
            source_path: "$.image".to_owned(),
            occurrence: 0,
        }
    );
}

#[test]
fn a_payload_that_no_longer_matches_offers_removal_first_and_adoption_second() {
    let plan = plan(&result(vec![Finding::new(
        codes::ALIAS_OBJECT_MISMATCH,
        "asset",
        "assets/portrait.png",
        "stored payload hash or size differs from the alias",
    )]));
    assert_eq!(plan.len(), 2);
    assert!(matches!(
        plan[0].action,
        RepairAction::DropAlias { .. } if plan[0].preferred
    ));
    assert!(matches!(
        plan[1].action,
        RepairAction::AdoptStoredPayload { .. } if !plan[1].preferred && !plan[1].discards
    ));
}

#[test]
fn a_finding_with_no_fixed_transformation_offers_nothing() {
    let plan = plan(&result(vec![
        Finding::new(codes::UNCLASSIFIED, "library", "", "unknown shape"),
        Finding::new(codes::OBJECT_UNREFERENCED, "asset", "abcd", "nothing uses it"),
        // A damaged storage class leaves no record to derive a repair from.
        Finding::new(
            codes::RECORD_INVALID,
            "asset_aliases",
            "assets/x",
            "portable SQL storage class mismatch",
        ),
    ]));
    assert!(plan.is_empty(), "{plan:?}");
}

#[test]
fn a_derived_value_rule_offers_recomputing_the_table_without_losing_anything() {
    let plan = plan(&result(vec![Finding::new(
        codes::RECORD_INVALID,
        "conversations",
        "",
        "portable conversation message count differs",
    )]));
    let [candidate] = plan.as_slice() else {
        panic!("one candidate: {plan:?}");
    };
    assert!(candidate.preferred && !candidate.discards);
    assert_eq!(
        candidate.action,
        RepairAction::NormalizeRecords {
            table: "conversations".to_owned(),
        }
    );
}

#[test]
fn an_orphan_offers_restoring_its_owner_rather_than_deleting_it() {
    let plan = plan(&result(vec![Finding::new(
        codes::RECORD_ORPHAN,
        "messages",
        "",
        "portable message has no conversation",
    )]));
    let [candidate] = plan.as_slice() else {
        panic!("one candidate: {plan:?}");
    };
    assert!(!candidate.discards);
    assert_eq!(
        candidate.action,
        RepairAction::RecoverOrphans {
            table: "messages".to_owned(),
        }
    );
}

#[test]
fn the_preview_counts_what_the_selection_changes_and_what_it_leaves() {
    let diagnosis = result(vec![
        reference(),
        Finding::new(
            codes::ALIAS_OBJECT_ABSENT,
            "asset",
            "assets/gone.png",
            "no stored payload binds to the alias",
        ),
        Finding::new(codes::UNCLASSIFIED, "library", "", "unknown shape"),
    ]);
    let offered = plan(&diagnosis);
    let selection: Vec<String> = offered.iter().map(|candidate| candidate.id.clone()).collect();
    let preview = preview(&diagnosis, &selection);
    assert_eq!(preview.answered, 2);
    assert_eq!(preview.remaining, 1);
    assert_eq!(preview.dropped_references, 1);
    assert_eq!(preview.dropped_aliases, 1);
    assert_eq!(preview.discarding.len(), 2);
    assert!(preview.proposes_snapshot, "a blocking repair proposes one");
}

#[test]
fn only_a_candidate_the_diagnosis_offers_can_be_selected() {
    let diagnosis = result(vec![reference()]);
    let preview = preview(
        &diagnosis,
        &["99:drop-everything".to_owned(), "0:drop-reference".to_owned()],
    );
    assert_eq!(preview.selected.len(), 1);
    assert_eq!(preview.selected[0].id, "0:drop-reference");
}

#[test]
fn a_small_change_that_blocks_nothing_does_not_propose_a_snapshot() {
    let diagnosis = result(vec![reference()]);
    assert_eq!(diagnosis.items[0].severity, Severity::Degraded);
    let preview = preview(&diagnosis, &["0:drop-reference".to_owned()]);
    assert!(!preview.proposes_snapshot);
}
