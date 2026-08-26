#[path = "../src/asset_repository/mod.rs"]
mod asset_repository;

use asset_repository::{PayloadCas, PreparedPayload};
use std::io::{self, Read};
use std::sync::{Arc, Barrier};

#[test]
fn stores_exact_bytes_at_the_sha256_shard_path() {
    let directory = tempfile::tempdir().expect("temporary repository");
    let cas = PayloadCas::new(directory.path());

    let prepared = cas.prepare_bytes(b"abc").expect("prepare payload");

    assert_eq!(
        prepared.content_hash,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(prepared.byte_size, 3);
    assert_eq!(
        prepared.physical_key,
        "assets-v2/objects/ba/7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert!(!prepared.deduplicated);
    assert_eq!(
        std::fs::read(directory.path().join(&prepared.physical_key)).expect("stored payload"),
        b"abc"
    );
}

#[test]
fn zero_byte_duplicates_reuse_the_existing_immutable_object() {
    let directory = tempfile::tempdir().expect("temporary repository");
    let cas = PayloadCas::new(directory.path());

    let first = cas.prepare_bytes(b"").expect("first prepare");
    let second = cas.prepare_bytes(b"").expect("duplicate prepare");

    assert_eq!(
        first.content_hash,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(first.byte_size, 0);
    assert!(!first.deduplicated);
    assert_eq!(
        second,
        PreparedPayload {
            deduplicated: true,
            ..first
        }
    );
    assert_eq!(
        std::fs::metadata(directory.path().join(&second.physical_key))
            .expect("zero-byte object")
            .len(),
        0
    );
}

#[test]
fn rejects_an_existing_target_with_different_exact_bytes() {
    let directory = tempfile::tempdir().expect("temporary repository");
    let cas = PayloadCas::new(directory.path());
    let first = cas.prepare_bytes(b"abc").expect("initial prepare");
    let object_path = directory.path().join(&first.physical_key);
    std::fs::write(&object_path, b"abd").expect("corrupt existing object");

    let error = cas
        .prepare_bytes(b"abc")
        .expect_err("corruption must be rejected");

    assert!(error.to_string().contains("collision or corruption"));
    assert_eq!(
        std::fs::read(object_path).expect("corrupt target remains visible"),
        b"abd"
    );
}

struct InterruptedReader {
    yielded: bool,
}

impl Read for InterruptedReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.yielded {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "synthetic interruption",
            ));
        }
        self.yielded = true;
        buffer[..3].copy_from_slice(b"abc");
        Ok(3)
    }
}

#[test]
fn interrupted_stream_removes_its_unique_staging_file() {
    let directory = tempfile::tempdir().expect("temporary repository");
    let cas = PayloadCas::new(directory.path());
    let mut reader = InterruptedReader { yielded: false };

    let error = cas
        .prepare_reader(&mut reader)
        .expect_err("interrupted prepare");

    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    let staging = directory.path().join("assets-v2/staging");
    let staged_count = std::fs::read_dir(staging)
        .map(|entries| entries.count())
        .unwrap_or_default();
    assert_eq!(staged_count, 0);
}

#[test]
fn direct_stat_rejects_hashes_that_could_escape_the_owned_root() {
    let directory = tempfile::tempdir().expect("temporary repository");
    let cas = PayloadCas::new(directory.path());

    for invalid in [
        "../objects",
        "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD",
        "ba7816bf",
        "zz7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
    ] {
        let error = cas.stat_object(invalid).expect_err("invalid object hash");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}

#[test]
fn direct_stat_and_read_resolve_only_the_requested_hash_path() {
    let directory = tempfile::tempdir().expect("temporary repository");
    let cas = PayloadCas::new(directory.path());
    let prepared = cas
        .prepare_bytes(b"direct lookup")
        .expect("prepare payload");
    let missing = "0000000000000000000000000000000000000000000000000000000000000000";

    assert_eq!(
        cas.stat_object(&prepared.content_hash)
            .expect("stat object"),
        Some(13)
    );
    assert_eq!(
        cas.read_object(&prepared.content_hash)
            .expect("read object"),
        Some(b"direct lookup".to_vec())
    );
    assert_eq!(cas.stat_object(missing).expect("stat missing object"), None);
    assert_eq!(cas.read_object(missing).expect("read missing object"), None);
}

#[test]
fn concurrent_publish_race_creates_one_object_and_cleans_all_staging_files() {
    let directory = tempfile::tempdir().expect("temporary repository");
    let repository_root = directory.path().to_path_buf();
    let barrier = Arc::new(Barrier::new(8));
    let threads = (0..8)
        .map(|_| {
            let repository_root = repository_root.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let cas = PayloadCas::new(repository_root);
                barrier.wait();
                cas.prepare_bytes(b"race payload")
            })
        })
        .collect::<Vec<_>>();
    let prepared = threads
        .into_iter()
        .map(|thread| {
            thread
                .join()
                .expect("prepare thread")
                .expect("race prepare")
        })
        .collect::<Vec<_>>();

    assert_eq!(
        prepared
            .iter()
            .filter(|result| !result.deduplicated)
            .count(),
        1
    );
    assert!(prepared
        .iter()
        .all(|result| result.content_hash == prepared[0].content_hash));
    assert_eq!(
        std::fs::read(directory.path().join(&prepared[0].physical_key)).expect("race object"),
        b"race payload"
    );
    assert_eq!(
        std::fs::read_dir(directory.path().join("assets-v2/staging"))
            .expect("staging directory")
            .count(),
        0
    );
}
