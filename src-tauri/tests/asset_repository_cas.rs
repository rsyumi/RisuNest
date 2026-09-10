// payload_cas.rs reaches its shared trust-boundary helpers through
// crate::trust_boundary, so this standalone compilation provides the same
// module at the test-crate root.
#[path = "../src/trust_boundary.rs"]
mod trust_boundary;

#[path = "../src/asset_repository/payload_cas.rs"]
mod payload_cas;

use payload_cas::{PayloadCas, PreparedPayload};
use std::ffi::OsString;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

const RACE_ROOT_ENV: &str = "RISUNEST_CAS_RACE_ROOT";
const RACE_START_ENV: &str = "RISUNEST_CAS_RACE_START";
const RACE_READY_ENV: &str = "RISUNEST_CAS_RACE_READY";
const RACE_RESULT_ENV: &str = "RISUNEST_CAS_RACE_RESULT";

fn import_payload(root: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    use std::io::Write;
    let directory = root.join("native-file-jobs/jobs/synthetic-import");
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join(name);
    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
    path
}

#[test]
fn adopts_import_without_copying_and_preserves_conflicts_and_cancelled_sources() {
    use sha2::{Digest, Sha256};
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    let bytes = b"synthetic immutable payload";
    let hash = hex::encode(Sha256::digest(bytes));
    let path = import_payload(directory.path(), "first.payload", bytes);
    let prepared = cas
        .adopt_import_payload(&path, &hash, bytes.len() as u64, &|| false)
        .unwrap();
    assert!(!prepared.deduplicated);
    assert!(!path.exists());
    assert_eq!(cas.read_object(&hash).unwrap().unwrap(), bytes);
    let duplicate = import_payload(directory.path(), "second.payload", bytes);
    assert!(
        cas.adopt_import_payload(&duplicate, &hash, bytes.len() as u64, &|| false)
            .unwrap()
            .deduplicated
    );
    assert!(!duplicate.exists());
    let corrupt = import_payload(
        directory.path(),
        "corrupt.payload",
        &vec![0_u8; bytes.len()],
    );
    assert!(cas
        .adopt_import_payload(&corrupt, &hash, bytes.len() as u64, &|| false)
        .is_err());
    assert!(corrupt.exists());
    let linked = import_payload(directory.path(), "linked.payload", bytes);
    let alias = directory.path().join("external-alias");
    std::fs::hard_link(&linked, &alias).unwrap();
    assert!(cas
        .adopt_import_payload(&linked, &hash, bytes.len() as u64, &|| false)
        .is_err());
    assert!(linked.exists());
    assert_eq!(std::fs::read(&alias).unwrap(), bytes);
    let cancelled = import_payload(directory.path(), "cancelled.payload", bytes);
    assert_eq!(
        cas.adopt_import_payload(&cancelled, &hash, bytes.len() as u64, &|| true)
            .unwrap_err()
            .kind(),
        io::ErrorKind::Interrupted
    );
    assert!(cancelled.exists());
    let external = directory.path().join("user.payload");
    std::fs::write(&external, bytes).unwrap();
    assert!(cas
        .adopt_import_payload(&external, &hash, bytes.len() as u64, &|| false)
        .is_err());
    assert!(external.exists());
}

#[test]
#[ignore = "synthetic IO measurements"]
fn import_adoption_benchmark() {
    use sha2::{Digest, Sha256};
    for (count, size) in [(1000, 100 * 1024), (2500, 200 * 1024), (10000, 1024)] {
        for adoption in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let cas = PayloadCas::new(directory.path()).unwrap();
            let mut descriptors = Vec::new();
            for i in 0..count {
                let mut bytes = vec![42_u8; size];
                bytes[..8].copy_from_slice(&(i as u64).to_le_bytes());
                let hash = hex::encode(Sha256::digest(&bytes));
                descriptors.push((
                    import_payload(directory.path(), &format!("{i}.payload"), &bytes),
                    hash,
                ));
            }
            let started = Instant::now();
            for (path, hash) in &descriptors {
                let prepared = if adoption {
                    cas.adopt_import_payload(path, hash, size as u64, &|| false)
                        .unwrap()
                } else {
                    cas.prepare_reader_expected(
                        &mut std::fs::File::open(path).unwrap(),
                        hash,
                        size as u64,
                    )
                    .unwrap()
                };
                assert_eq!(&prepared.content_hash, hash);
                assert_eq!(prepared.byte_size, size as u64);
            }
            println!(
                "import-io count={count} bytes={} adoption={adoption} elapsed_ms={}",
                count * size,
                started.elapsed().as_millis()
            );
        }
    }
}

#[cfg(unix)]
fn symlink_file(original: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(original, link)
}

#[cfg(windows)]
fn symlink_file(original: &Path, link: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_file(original, link)
}

#[cfg(unix)]
fn symlink_directory(original: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(original, link)
}

#[cfg(windows)]
fn symlink_directory(original: &Path, link: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_dir(original, link)
}

#[test]
fn stores_exact_bytes_at_the_sha256_shard_path() {
    let directory = tempfile::tempdir().expect("temporary repository");
    let cas = PayloadCas::new(directory.path()).expect("open repository");

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
    assert_eq!(prepared.directory_entries_synced, cfg!(unix));
    assert_eq!(
        std::fs::read(directory.path().join(&prepared.physical_key)).expect("stored payload"),
        b"abc"
    );
}

#[test]
fn zero_byte_duplicates_reuse_the_existing_immutable_object() {
    let directory = tempfile::tempdir().expect("temporary repository");
    let cas = PayloadCas::new(directory.path()).expect("open repository");

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
    let cas = PayloadCas::new(directory.path()).expect("open repository");
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
    let cas = PayloadCas::new(directory.path()).expect("open repository");
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
    let cas = PayloadCas::new(directory.path()).expect("open repository");

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
    let cas = PayloadCas::new(directory.path()).expect("open repository");
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
fn exact_object_path_can_be_reopened_for_bounded_backup_streaming() {
    let directory = tempfile::tempdir().expect("temporary repository");
    let cas = PayloadCas::new(directory.path()).expect("open repository");
    let prepared = cas
        .prepare_bytes(b"streamed backup payload")
        .expect("prepare payload");

    let path = cas
        .object_path(&prepared.content_hash)
        .expect("resolve object")
        .expect("object exists");

    assert_eq!(
        std::fs::read(path).expect("reopen exact object"),
        b"streamed backup payload"
    );
    assert_eq!(
        cas.object_path(&"00".repeat(32))
            .expect("resolve missing object"),
        None
    );
}

#[test]
fn rejects_a_linked_object_target_without_reading_or_replacing_it() {
    let directory = tempfile::tempdir().expect("temporary repository");
    let external = tempfile::NamedTempFile::new().expect("external payload");
    std::fs::write(external.path(), b"abc").expect("write external payload");
    let hash = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    let object_path =
        directory
            .path()
            .join(format!("assets-v2/objects/{}/{}", &hash[..2], &hash[2..]));
    std::fs::create_dir_all(object_path.parent().expect("object parent"))
        .expect("create object parent");
    symlink_file(external.path(), &object_path).expect("link external payload");
    let cas = PayloadCas::new(directory.path()).expect("open repository");

    let prepare_error = cas
        .prepare_bytes(b"abc")
        .expect_err("linked object must not satisfy prepare");
    let stat_error = cas
        .stat_object(hash)
        .expect_err("linked object must not satisfy stat");
    let read_error = cas
        .read_object(hash)
        .expect_err("linked object must not satisfy read");

    assert_eq!(prepare_error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(stat_error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(read_error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(
        std::fs::read(external.path()).expect("external payload remains"),
        b"abc"
    );
}

#[test]
fn rejects_a_linked_repository_component_without_writing_outside_the_root() {
    let directory = tempfile::tempdir().expect("temporary repository");
    let external = tempfile::tempdir().expect("external directory");
    let assets_path = directory.path().join("assets-v2");
    symlink_directory(external.path(), &assets_path).expect("link external directory");
    let cas = PayloadCas::new(directory.path()).expect("open repository");

    let error = cas
        .prepare_bytes(b"abc")
        .expect_err("linked repository component must be rejected");

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(
        std::fs::read_dir(external.path())
            .expect("read external directory")
            .count(),
        0
    );
}

#[test]
fn rejects_a_repository_root_reached_through_a_link_component() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let external = tempfile::tempdir().expect("external repository");
    let linked_root = parent.path().join("linked-root");
    symlink_directory(external.path(), &linked_root).expect("link repository root");

    let error = PayloadCas::new(&linked_root).expect_err("linked root must be rejected");

    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(
        std::fs::read_dir(external.path())
            .expect("read external repository")
            .count(),
        0
    );
}

#[test]
fn rejects_a_repository_root_replaced_after_open_for_every_operation() {
    let parent = tempfile::tempdir().expect("temporary parent");
    let external = tempfile::tempdir().expect("external repository");
    let repository_root = parent.path().join("repository");
    let original_root = parent.path().join("original-repository");
    std::fs::create_dir(&repository_root).expect("create repository root");
    let cas = PayloadCas::new(&repository_root).expect("open repository");
    std::fs::rename(&repository_root, &original_root).expect("move original repository root");
    symlink_directory(external.path(), &repository_root).expect("replace repository root");
    let hash = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    let prepare_error = cas
        .prepare_bytes(b"abc")
        .expect_err("replaced root must reject prepare");
    let stat_error = cas
        .stat_object(hash)
        .expect_err("replaced root must reject stat");
    let read_error = cas
        .read_object(hash)
        .expect_err("replaced root must reject read");

    assert_eq!(prepare_error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(stat_error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(read_error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(
        std::fs::read_dir(external.path())
            .expect("read external repository")
            .count(),
        0
    );
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
                let cas = PayloadCas::new(repository_root).expect("open repository");
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

#[test]
fn cross_process_publish_child() {
    let Some(repository_root) = std::env::var_os(RACE_ROOT_ENV) else {
        return;
    };
    let start = PathBuf::from(std::env::var_os(RACE_START_ENV).expect("race child start path"));
    let ready = PathBuf::from(std::env::var_os(RACE_READY_ENV).expect("race child ready path"));
    let result = PathBuf::from(std::env::var_os(RACE_RESULT_ENV).expect("race child result path"));
    std::fs::write(&ready, b"ready").expect("announce race child readiness");
    wait_for_path(&start, "race start signal");

    let cas = PayloadCas::new(repository_root).expect("open repository");
    let payload = vec![0x5a; 4 * 1024 * 1024];
    let prepared = cas.prepare_bytes(&payload).expect("child prepare");
    std::fs::write(
        result,
        if prepared.deduplicated {
            b"duplicate".as_slice()
        } else {
            b"created".as_slice()
        },
    )
    .expect("write race child result");
}

#[test]
fn cross_process_publish_race_creates_exactly_one_object() {
    let directory = tempfile::tempdir().expect("temporary repository");
    let coordination = tempfile::tempdir().expect("temporary coordination directory");
    let executable = std::env::current_exe().expect("current test executable");
    let start = coordination.path().join("start");
    let mut ready_paths = Vec::new();
    let mut result_paths = Vec::new();
    let mut children = Vec::new();

    for index in 0..8 {
        let ready = coordination.path().join(format!("ready-{index}"));
        let result = coordination.path().join(format!("result-{index}"));
        children.push(spawn_race_child(
            &executable,
            directory.path().as_os_str().to_owned(),
            &start,
            &ready,
            &result,
        ));
        ready_paths.push(ready);
        result_paths.push(result);
    }

    wait_for_paths(&ready_paths, "race children readiness");
    std::fs::write(&start, b"start").expect("release race children");
    wait_for_children(children);

    let results = result_paths
        .iter()
        .map(|path| std::fs::read_to_string(path).expect("race child result"))
        .collect::<Vec<_>>();
    assert_eq!(
        results.iter().filter(|result| *result == "created").count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| *result == "duplicate")
            .count(),
        7
    );
    assert_eq!(
        std::fs::read_dir(directory.path().join("assets-v2/staging"))
            .expect("cross-process staging directory")
            .count(),
        0
    );
}

fn spawn_race_child(
    executable: &Path,
    repository_root: OsString,
    start: &Path,
    ready: &Path,
    result: &Path,
) -> Child {
    Command::new(executable)
        .arg("cross_process_publish_child")
        .arg("--exact")
        .arg("--nocapture")
        .env(RACE_ROOT_ENV, repository_root)
        .env(RACE_START_ENV, start)
        .env(RACE_READY_ENV, ready)
        .env(RACE_RESULT_ENV, result)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn race child")
}

fn wait_for_path(path: &Path, description: &str) {
    wait_for_paths(&[path.to_path_buf()], description);
}

fn wait_for_paths(paths: &[PathBuf], description: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !paths.iter().all(|path| path.exists()) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {description}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_children(children: Vec<Child>) {
    for child in children {
        let output = child.wait_with_output().expect("wait for race child");
        assert!(
            output.status.success(),
            "race child failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
