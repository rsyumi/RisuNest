use risunest_sync_wire::{canonical, descriptor::*, hash, MAX_METADATA_BYTES};
use std::collections::BTreeMap;

#[test]
fn inserting_a_reference_preserves_pages_across_a_sorted_hash_inventory() {
    let mut values: Vec<_> = (0..100_000u64)
        .map(|n| hash(format!("synthetic unique asset body {n:016}").as_bytes()))
        .collect();
    values.sort();
    let (_, original) = build_reference_tree(&values, false).unwrap();
    let original: BTreeMap<_, _> = original.into_iter().collect();
    for position in [1, 50_000, 99_998] {
        let mut edited = values.clone();
        edited.remove(position);
        let (root, pages) = build_reference_tree(&edited, false).unwrap();
        let changed = pages
            .iter()
            .filter(|(h, _)| !original.contains_key(h))
            .count();
        assert!(
            changed <= 8,
            "one deletion at {position} rebuilt {changed} pages"
        );
        let pages: BTreeMap<_, _> = pages.into_iter().collect();
        let mut restored = Vec::new();
        visit_reference_tree(
            root.as_ref().unwrap(),
            false,
            |h| Ok(pages[h].clone()),
            |v, page| {
                if !page {
                    restored.push(v.to_owned());
                }
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(restored, edited);
    }
}

#[test]
fn half_million_dependencies_use_bounded_immutable_pages_and_small_root() {
    let values: Vec<_> = (0..500_000u64).map(|n| format!("{n:064x}")).collect();
    let (root, pages) = build_reference_tree(&values, false).unwrap();
    assert!(pages
        .iter()
        .all(|(_, bytes)| bytes.len() <= MAX_METADATA_BYTES));
    let pages: BTreeMap<_, _> = pages.into_iter().collect();
    let mut count = 0;
    visit_reference_tree(
        root.as_ref().unwrap(),
        false,
        |hash| Ok(pages[hash].clone()),
        |value, page| {
            if !page {
                assert_eq!(value, values[count]);
                count += 1;
            }
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(count, values.len());
    let descriptor = RecordDescriptor {
        object_hash: hash(b"content"),
        dependency_root: root,
        relation_root: None,
        scopes: vec![],
    };
    assert!(descriptor.bytes().unwrap().len() < 256);
}
#[test]
fn maximal_legal_keys_are_paged_by_bytes_not_only_count() {
    let values: Vec<_> = (0..40)
        .map(|i| format!("{i:03}{}", "한".repeat(21000)))
        .collect();
    let (root, pages) = build_reference_tree(&values, true).unwrap();
    assert!(pages.len() > 2);
    assert!(pages.iter().all(|(_, b)| b.len() <= MAX_METADATA_BYTES));
    let pages: BTreeMap<_, _> = pages.into_iter().collect();
    let mut found = Vec::new();
    visit_reference_tree(
        root.as_ref().unwrap(),
        true,
        |h| Ok(pages[h].clone()),
        |v, p| {
            if !p {
                found.push(v.to_owned());
            }
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(found, values);
}
#[test]
fn bad_hash_wrong_kind_duplicate_and_unsorted_references_fail() {
    assert!(build_reference_tree(&[hash(b"x"), hash(b"x")], false).is_err());
    let (root, pages) = build_reference_tree(&[hash(b"x")], false).unwrap();
    assert!(visit_reference_tree(
        root.as_ref().unwrap(),
        true,
        |_| Ok(pages[0].1.clone()),
        |_, _| Ok(())
    )
    .is_err());
    assert!(visit_reference_tree(
        root.as_ref().unwrap(),
        false,
        |_| Ok(b"wrong".to_vec()),
        |_, _| Ok(())
    )
    .is_err());
    let page = ReferencePage::Branches {
        children: vec![root.clone().unwrap(), root.clone().unwrap()],
    };
    let bytes = canonical::encode(&page).unwrap();
    let top = hash(&bytes);
    assert!(visit_reference_tree(
        &top,
        false,
        |h| Ok(if h == top {
            bytes.clone()
        } else {
            pages[0].1.clone()
        }),
        |_, _| Ok(())
    )
    .is_err());
}
