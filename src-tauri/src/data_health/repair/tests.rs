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
fn a_missing_chat_page_offers_selecting_the_first_conversation_without_discarding_data() {
    let candidates = plan(&result(vec![reference().at("$.chatPage", 0)]));
    assert_eq!(candidates.len(), 1);
    assert!(candidates[0].preferred && !candidates[0].discards);
    assert_eq!(candidates[0].action, RepairAction::SelectFirstConversation { character_id: "char-1".into() });
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
            token: None,
        }
    );
}

#[test]
fn a_reference_removal_takes_its_whole_entry_and_cuts_only_a_numbered_inlay() {
    let inlay = || reference().at("$.firstMessage", 1).targeting("inlay", "gone");
    let message = Finding::new(codes::REFERENCE_MISSING, "conversation", "char-1/chat-1", "")
        .at("$.message[7].data", 0)
        .targeting("inlay", "gone")
        .in_token(0);
    let plan = plan(&result(vec![
        reference().at("$.ccAssets[2].uri", 0),
        // A diagnosis written before tokens were numbered.
        inlay(),
        inlay().in_token(3),
        message,
    ]));
    let removals: Vec<_> = plan
        .iter()
        .map(|candidate| match &candidate.action {
            RepairAction::DropReference { owner, source_path, token, .. } => {
                (candidate.finding, owner.kind.as_str(), owner.id.as_str(), source_path.as_str(), *token)
            }
            other => panic!("unexpected action: {other:?}"),
        })
        .collect();
    assert_eq!(
        removals,
        [
            (0, "character", "char-1", "$.ccAssets[2]", None),
            (2, "character", "char-1", "$.firstMessage", Some(3)),
            (3, "message", "char-1/chat-1/7", "$.data", Some(0)),
        ]
    );
}

#[test]
fn an_owner_inside_the_root_is_offered_only_when_one_entry_carries_it() {
    let inside = |kind: &str, id: &str, path: &str, target: &str| {
        Finding::new(codes::REFERENCE_MISSING, kind, id, "").at(path, 0).targeting(target, "gone")
    };
    let plan = plan(&result(vec![
        inside("persona", "persona-a", "$.icon", "asset").in_sole_entry(),
        inside("persona", "persona-a", "$.embeddedModule.assets[2][1]", "asset").in_sole_entry(),
        inside("module", "module-a", "$.assets[1][1]", "asset").in_sole_entry(),
        inside("module", "module-a", "$.lorebook[0].content", "inlay").in_token(1).in_sole_entry(),
        inside("loadout", "Loadout", "$.characterIds[0]", "character").in_sole_entry(),
        inside("root", "database", "$.customBackground", "asset"),
        // No entry or several carry this identity, or the diagnosis was written before the mark.
        inside("persona", "persona-b", "$.icon", "asset"),
        // Removing these leaves the finding in place.
        inside("persona", "persona-a", "$.embeddedModule.id", "module").in_sole_entry(),
        inside("loadout", "Loadout", "$.presetName", "preset").in_sole_entry(),
        inside("loadout", "Loadout", "$.personaId", "persona").in_sole_entry(),
        inside("root", "database", "$.botPresetsId", "preset"),
        inside("root", "database", "$.selectedPersona", "persona"),
        inside("folder", "folder-a", "$.img", "asset").in_sole_entry(),
    ]));
    let removals: Vec<_> = plan
        .iter()
        .map(|candidate| match &candidate.action {
            RepairAction::DropReference { owner, source_path, token, .. } => {
                (candidate.finding, owner.kind.as_str(), owner.id.as_str(), source_path.as_str(), *token)
            }
            other => panic!("unexpected action: {other:?}"),
        })
        .collect();
    assert_eq!(
        removals,
        [
            (0, "persona", "persona-a", "$.icon", None),
            (1, "persona", "persona-a", "$.embeddedModule.assets[2]", None),
            (2, "module", "module-a", "$.assets[1]", None),
            (3, "module", "module-a", "$.lorebook[0].content", Some(1)),
            (4, "loadout", "Loadout", "$.characterIds[0]", None),
            (5, "root", "database", "$.customBackground", None),
        ]
    );
}

#[test]
fn a_root_entry_is_named_only_by_an_identity_one_entry_carries_itself() {
    let root = serde_json::json!({
        "personas": [{ "id": "a" }, { "name": "no id" }, { "id": "dup" }, { "id": "dup" }, { "id": "#1" }, {}, { "id": "#9" }],
        "modules": [{ "name": "no id" }, { "id": "m" }],
        "loadouts": [{ "id": "loadout-id", "name": "Loadout" }],
    });
    let entry = |kind: &str, id: &str| {
        root_entry(&root, &Owner { kind: kind.to_owned(), id: id.to_owned() })
    };
    assert_eq!(entry("persona", "a"), Some(("personas", 0)));
    assert_eq!(entry("module", "m"), Some(("modules", 1)));
    assert_eq!(entry("loadout", "Loadout"), Some(("loadouts", 0)), "a loadout is named by its name");
    assert_eq!(entry("loadout", "loadout-id"), None);
    assert_eq!(entry("persona", "dup"), None, "several entries carry it");
    assert_eq!(entry("persona", "gone"), None);
    // A persona without an id is named by its position, which another entry can take, and an id
    // that spells a position is shared with the persona at that position.
    assert_eq!(entry("persona", "#5"), None);
    assert_eq!(entry("persona", "#1"), None);
    assert_eq!(entry("persona", "#9"), Some(("personas", 6)));
    assert_eq!(entry("module", ""), None, "a module without an id is named by nothing");
    assert_eq!(entry("character", "a"), None);
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
fn separated_root_material_offers_removing_the_duplicate_fields() {
    let plan = plan(&result(vec![Finding::new(
        codes::RECORD_INVALID,
        "root",
        "",
        "portable root contains separated field: pluginStorageMeta",
    )]));
    let [candidate] = plan.as_slice() else {
        panic!("one candidate: {plan:?}");
    };
    assert!(candidate.preferred && !candidate.discards);
    assert_eq!(
        candidate.action,
        RepairAction::NormalizeRecords {
            table: "root".to_owned(),
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
