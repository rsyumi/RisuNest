use risunest_sync_wire::{delta, hash, payload};
use std::collections::BTreeMap;
fn build(bytes: &[u8]) -> (payload::Payload, BTreeMap<String, Vec<u8>>) {
    let mut objects = BTreeMap::new();
    let payload = payload::build(&mut std::io::Cursor::new(bytes), |bytes| {
        objects.insert(hash(bytes), bytes.to_vec());
        Ok(())
    })
    .unwrap();
    (payload, objects)
}
#[test]
fn segmented_payload_round_trips_empty_binary_and_large_duplicate_content() {
    for bytes in [vec![], vec![0xff, 0, 12], vec![b'a'; 10 * 1024 * 1024]] {
        let (payload, objects) = build(&bytes);
        let mut restored = Vec::new();
        payload::restore(&payload, |hash| Ok(objects[hash].clone()), &mut restored).unwrap();
        assert_eq!(restored, bytes);
        assert!(objects.values().all(|b| b.len() <= payload::MAX_CHUNK));
        let mut wrong = payload.clone();
        wrong.content_hash = hash(b"wrong");
        assert!(
            payload::restore(&wrong, |hash| Ok(objects[hash].clone()), &mut Vec::new()).is_err()
        );
    }
}
#[test]
fn ten_megabyte_insertion_preserves_exact_bytes_with_small_changed_segments() {
    let base = vec![b'x'; 10 * 1024 * 1024];
    let mut target = base.clone();
    target.splice(
        5 * 1024 * 1024..5 * 1024 * 1024,
        b"one-new-message".iter().copied(),
    );
    let (_, old) = build(&base);
    let (payload, new) = build(&target);
    let mut transferred = 0;
    for (hash, bytes) in &new {
        if !old.contains_key(hash) {
            let bases = old
                .values()
                .filter(|b| b.len() >= payload::MIN_CHUNK)
                .map(Vec::as_slice)
                .take(4)
                .collect::<Vec<_>>();
            let patch = delta::create(&bases, bytes).and_then(|r| r.encode()).ok();
            transferred += patch
                .map(|p| p.len().min(bytes.len()))
                .unwrap_or(bytes.len());
        }
    }
    assert!(transferred < 16 * 1024);
    let mut output = Vec::new();
    payload::restore(&payload, |hash| Ok(new[hash].clone()), &mut output).unwrap();
    assert_eq!(output, target);
}
