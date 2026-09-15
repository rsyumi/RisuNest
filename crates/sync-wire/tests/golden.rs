use risunest_sync_wire::{
    batch, canonical, change_digest::ChangeDigest, hash, operation_id, ChangeSet, CommitIntent,
    Domain, ReadFence, RecordChange, RecordVersion, RemoteHead, Sequence,
};
use serde_json::{json, Value};

#[test]
fn shared_jcs_vectors_match_exact_utf8() {
    let vectors: Vec<Value> = serde_json::from_str(include_str!("golden.json")).unwrap();
    for vector in vectors {
        let expected = vector["canonical"].as_str().unwrap().as_bytes();
        assert_eq!(canonical::encode(&vector["input"]).unwrap(), expected);
        let decoded: Value = canonical::decode(expected, 1024).unwrap();
        assert_eq!(decoded, vector["input"]);
    }
}
#[test]
fn rejects_ambiguous_or_out_of_profile_control_json() {
    for bad in [
        r#"{"a":"x","a":"y"}"#,
        r#"{"nested":{"a":null,"a":true}}"#,
        r#""\ud800""#,
        r#""\udc00""#,
        "1",
        "1.0",
        "-0",
        "1e0",
        "9007199254740993",
        "NaN",
        "{}{}",
    ] {
        assert!(
            canonical::decode::<Value>(bad.as_bytes(), 1024).is_err(),
            "{bad}"
        );
    }
    assert!(canonical::decode::<Value>(&[0xff], 1024).is_err());
    assert!(canonical::decode::<Value>(b"{}", 1).is_err());
    assert!(canonical::encode(&json!({"size":1})).is_err());
}
#[test]
fn sequence_is_decimal_not_a_local_revision_or_js_number() {
    let a: Sequence = "9007199254740993".to_string().try_into().unwrap();
    assert_eq!(a.next().unwrap().as_str(), "9007199254740994");
    assert!(Sequence::from(10) > Sequence::from(9));
    assert_eq!(Sequence::from(999).next().unwrap(), Sequence::from(1000));
    for bad in ["", "01", "-1", "1.0", "1e1", " 1", "+1"] {
        assert!(Sequence::try_from(bad.to_string()).is_err());
    }
    assert!(Sequence::try_from("9".repeat(64)).unwrap().next().is_err());
}
fn changes() -> ChangeSet {
    ChangeSet {
        changes: vec![RecordChange {
            domain: Domain::Library,
            key: "conversation/한글".into(),
            before: RecordVersion::Absent,
            after: RecordVersion::Live {
                object_hash: hash(b"raw"),
                descriptor_hash: None,
            },
        }],
        read_fences: vec![],
        scope_fences: vec![],
    }
}
fn intent() -> CommitIntent {
    CommitIntent {
        device_operation_seq: 1.into(),
        expected_head: RemoteHead {
            seq: 7.into(),
            head_id: hash(b"head"),
            ..RemoteHead::genesis("library".into(), "epoch".into()).unwrap()
        },
        changes_digest: changes().digest().unwrap(),
        staged_changes_id: "first".into(),
    }
}
#[test]
fn intent_identity_excludes_transport_and_staging_but_includes_fences() {
    let a = intent();
    let mut b = a.clone();
    b.staged_changes_id = "second".into();
    assert_eq!(a.digest().unwrap(), b.digest().unwrap());
    b.expected_head.seq = 8.into();
    assert_ne!(a.digest().unwrap(), b.digest().unwrap());
    let mut changed = changes();
    changed.read_fences.push(ReadFence {
        domain: Domain::Library,
        key: "owner".into(),
        version: RecordVersion::Absent,
    });
    assert_ne!(changed.digest().unwrap(), changes().digest().unwrap());
    assert_ne!(
        operation_id("a-b", "c", &1.into()).unwrap(),
        operation_id("a", "b-c", &1.into()).unwrap()
    );
    assert_ne!(
        operation_id("a", "b", &1.into()).unwrap(),
        operation_id("a", "b", &2.into()).unwrap()
    );
}
#[test]
fn change_schema_rejects_duplicate_keys_absent_targets_unknown_fields_and_bad_hashes() {
    let mut c = changes();
    c.changes.push(c.changes[0].clone());
    assert!(c.validate().is_err());
    let mut c = changes();
    c.changes[0].after = RecordVersion::Absent;
    assert!(c.validate().is_err());
    let mut c = changes();
    c.changes[0].key = "x".repeat(64 * 1024);
    assert!(c.validate().is_ok());
    c.changes[0].key.push('x');
    assert!(c.validate().is_err());
    assert!(
        canonical::decode::<RecordVersion>(br#"{"state":"absent","extra":true}"#, 1024).is_err()
    );
    assert!(RecordVersion::Live {
        object_hash: "../bad".into(),
        descriptor_hash: None
    }
    .validate()
    .is_err());
    assert!(RecordVersion::Live {
        object_hash: hash(b"x"),
        descriptor_hash: Some("invalid".into())
    }
    .validate()
    .is_err());
}
#[test]
fn full_batch_preserves_opaque_bytes_including_empty_and_noncanonical_json() {
    let raw = br#"{ "b":1.0, "a":9007199254740993 }"#;
    let encoded = batch::encode(&[b"", raw, &[0xff, 0, 7]]).unwrap();
    let decoded = batch::decode(&encoded).unwrap();
    assert_eq!(decoded[0].bytes, b"");
    assert_eq!(decoded[1].bytes, raw);
    assert_eq!(decoded[2].bytes, &[0xff, 0, 7]);
    assert_eq!(decoded[1].hash, hash(raw));
}
#[test]
fn batch_rejects_every_truncated_prefix_trailing_bytes_corruption_and_unknown_codec() {
    let encoded = batch::encode(&[b"data"]).unwrap();
    for end in 0..encoded.len() {
        assert!(batch::decode(&encoded[..end]).is_err(), "prefix {end}");
    }
    let mut bad = encoded.clone();
    bad.push(0);
    assert!(batch::decode(&bad).is_err());
    let mut bad = encoded.clone();
    *bad.last_mut().unwrap() ^= 1;
    assert!(batch::decode(&bad).is_err());
    let mut bad = encoded.clone();
    bad[8] = 1;
    assert!(batch::decode(&bad).is_err());
    let mut bad = encoded.clone();
    bad[41..49].fill(255);
    assert!(batch::decode(&bad).is_err());
    assert!(batch::encode(&[&vec![0; batch::MAX_BATCH_BYTES]]).is_err());
    assert!(batch::encode(&vec![b"".as_slice(); 1025]).is_err());
}

#[test]
fn domain_ordering_matches_its_wire_strings() {
    let mut sorted = Domain::ALL;
    sorted.sort_by_key(Domain::as_str);
    assert_eq!(sorted, Domain::ALL);
    assert_eq!(
        Domain::ALL.map(|d| d.as_str()),
        ["hypa", "library", "local-plugins"]
    );
    for domain in Domain::ALL {
        assert_eq!(Domain::try_from(domain.as_str()).unwrap(), domain);
        assert_eq!(
            canonical::encode(&domain).unwrap(),
            format!("\"{domain}\"").as_bytes()
        );
    }
    assert!(Domain::try_from("device-settings").is_err());
    assert!(canonical::decode::<Domain>(b"\"device-settings\"", 1024).is_err());
}
#[test]
fn change_sets_address_records_by_domain_and_key() {
    let mut c = changes();
    let mut other = c.changes[0].clone();
    other.domain = Domain::Hypa;
    c.changes.insert(0, other);
    assert!(c.validate().is_ok());
    c.changes.swap(0, 1);
    assert!(c.validate().is_err());
    let mut c = changes();
    c.changes.push(RecordChange {
        domain: Domain::Hypa,
        ..c.changes[0].clone()
    });
    // hypa sorts before library, so the same key in two domains still needs order.
    assert!(c.validate().is_err());
    let mut c = changes();
    let library = c.changes[0].clone();
    c.changes[0].domain = Domain::Hypa;
    assert_ne!(c.digest().unwrap(), changes().digest().unwrap());
    c.changes.push(library);
    assert!(c.validate().is_ok());
}
#[test]
fn streaming_digest_equals_the_flat_change_set() {
    let mut set = changes();
    set.changes.insert(
        0,
        RecordChange {
            domain: Domain::Hypa,
            ..set.changes[0].clone()
        },
    );
    set.read_fences.push(ReadFence {
        domain: Domain::LocalPlugins,
        key: "owner".into(),
        version: RecordVersion::Absent,
    });
    set.read_fences.insert(
        0,
        ReadFence {
            domain: Domain::Hypa,
            key: "owner".into(),
            version: RecordVersion::Absent,
        },
    );
    let mut digest = ChangeDigest::new();
    for change in &set.changes {
        digest.change(change).unwrap();
    }
    for fence in &set.read_fences {
        digest.read_fence(fence).unwrap();
    }
    assert_eq!(digest.finish().unwrap(), set.digest().unwrap());
}
#[test]
fn heads_carry_one_sequence_and_a_state_per_section() {
    let head = RemoteHead::genesis("library".into(), "epoch".into()).unwrap();
    head.validate().unwrap();
    assert_eq!(head.sections.len(), 3);
    let ids: std::collections::BTreeSet<_> = head
        .sections
        .values()
        .map(|section| section.state_id.clone())
        .collect();
    assert_eq!(ids.len(), 3);
    let encoded = canonical::encode(&head).unwrap();
    assert_eq!(
        canonical::decode::<RemoteHead>(&encoded, 4096).unwrap(),
        head
    );
    assert!(std::str::from_utf8(&encoded)
        .unwrap()
        .contains("\"local-plugins\":"));
    let mut missing = head.clone();
    missing.sections.remove(&Domain::Hypa);
    assert!(missing.validate().is_err());
    let mut ahead = head.clone();
    ahead.sections.get_mut(&Domain::Hypa).unwrap().changed_seq = 1.into();
    assert!(ahead.validate().is_err());
    let mut reclaimed = head.clone();
    reclaimed.min_retained_seq = 0.into();
    reclaimed.sections.get_mut(&Domain::Hypa).unwrap().gc_floor = 1.into();
    assert!(reclaimed.validate().is_err());
}
