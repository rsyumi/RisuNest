use super::*;
use crate::local_backup::NeverCancelled;
use crate::native_file_jobs::{JobKind, JobRegistry};
use std::io::Cursor;

const V110: &[u8] = include_bytes!("fixtures/pocket-risu-v1.10.0.bin");
const V112: &[u8] = include_bytes!("fixtures/pocket-risu-v1.12.0.bin");

struct StoreSink {
    store: Mutex<PersistentStore>,
    payloads: PreparedLegacyRestorePayloads,
    durable: Mutex<DurableCasJob>,
}

impl restore::ReplacementSink for StoreSink {
    fn begin(&self) -> StoreResult<StagingResult> {
        self.store.lock().unwrap().replace_begin()
    }
    fn put_root(&self, id: &str, root: &Value) -> StoreResult<()> {
        self.store.lock().unwrap().replace_put_root(id, root)
    }
    fn put_presets(&self, id: &str, presets: &[Value]) -> StoreResult<()> {
        self.store.lock().unwrap().replace_put_presets(id, presets)
    }
    fn add_characters(&self, id: &str, characters: &[Value]) -> StoreResult<()> {
        self.store
            .lock()
            .unwrap()
            .replace_add_characters(id, characters)
    }
    fn commit(&self, id: &str, revision: i64) -> StoreResult<RevisionResult> {
        let mut store = self.store.lock().unwrap();
        store.replace_put_asset_aliases(id, &self.payloads.asset_aliases)?;
        store.replace_put_cold_aliases(id, &self.payloads.cold_aliases)?;
        store.replace_put_asset_repository_authority(
            id,
            &AssetRepositoryAuthorityState::V2 {
                migration_id: "synthetic-pocket-restore".to_owned(),
                compatibility_hash: payload_compatibility_hash(&self.payloads),
            },
        )?;
        self.durable
            .lock()
            .unwrap()
            .seal(&mut store, now_millis())
            .map_err(crate::persistent_store::StoreError::from)?;
        store.replace_commit(id, Some(revision))
    }
    fn abort(&self, id: &str) -> StoreResult<()> {
        self.store.lock().unwrap().replace_abort(id)
    }
}

struct Import<'a> {
    root: &'a std::path::Path,
    job: &'a JobControl,
    fail_database: bool,
}

impl StrictLocalBackupDatabaseRestore for Import<'_> {
    fn restore_database(
        &mut self,
        database: &StagedLocalBackupEntry,
        entries: &[StagedLocalBackupEntry],
    ) -> Result<(), LocalBackupError> {
        let cas = PayloadCas::new(self.root).unwrap();
        let mut durable = DurableCasJob::begin(
            self.root,
            &self.job.id(),
            CasJobKind::LocalBackupRestore,
            now_millis(),
        )
        .unwrap();
        let payloads = prepare_legacy_restore_payloads(
            entries,
            &cas,
            &mut durable,
            &JobCancellation(self.job),
        )?;
        let sink = StoreSink {
            store: Mutex::new(PersistentStore::open(self.root).unwrap()),
            payloads,
            durable: Mutex::new(durable),
        };
        // Old data and aliases are still authoritative after every archive payload was prepared.
        assert_eq!(
            sink.store.lock().unwrap().read_root(None).unwrap().value["username"],
            "Existing data"
        );
        assert!(sink
            .store
            .lock()
            .unwrap()
            .read_asset_alias("inlay", "picture", None)
            .unwrap()
            .is_none());
        if self.fail_database {
            std::fs::write(database.staged_path.as_ref().unwrap(), b"invalid database").unwrap();
        }
        let result = restore::restore_started_risu_save_with_pre_activation(
            OpenedJobSource {
                file: open_staged(database)?,
                total_bytes: database.byte_length,
            },
            1,
            self.job,
            &sink,
            || Ok(None),
        );
        sink.durable
            .lock()
            .unwrap()
            .release(if result.is_ok() {
                CasReleaseOutcome::Committed
            } else {
                CasReleaseOutcome::Aborted
            })
            .unwrap();
        result.map(|_| ()).map_err(|error| invalid(error.message))
    }
}

fn seed(root: &std::path::Path) {
    let mut store = PersistentStore::open(root).unwrap();
    let id = store.replace_begin().unwrap().staging_id;
    store
        .replace_put_root(&id, &serde_json::json!({"username":"Existing data"}))
        .unwrap();
    store.replace_put_presets(&id, &[]).unwrap();
    store.replace_add_characters(&id, &[]).unwrap();
    store.replace_commit(&id, Some(0)).unwrap();
}

#[test]
fn both_pocket_versions_restore_database_and_media_into_the_persistent_store() {
    for archive in [V110, V112] {
        let directory = tempfile::tempdir().unwrap();
        seed(directory.path());
        let job = JobRegistry::default()
            .create(JobKind::RestoreLegacyLocalBackup)
            .unwrap();
        job.start(JobPhase::ReadingSource).unwrap();
        parse_legacy_local_backup_v1(
            &mut Cursor::new(archive),
            directory.path(),
            PayloadTarget::JobStaging,
            &mut Import {
                root: directory.path(),
                job: &job,
                fail_database: false,
            },
            &NeverCancelled,
        )
        .unwrap();
        let store = PersistentStore::open(directory.path()).unwrap();
        let actual = store.materialize(None).unwrap();
        let expected: Value = serde_json::from_str(include_str!("fixtures/expected.json")).unwrap();
        for key in [
            "username",
            "characters",
            "botPresets",
            "modules",
            "plugins",
            "pluginCustomStorage",
            "loadouts",
        ] {
            assert_eq!(actual[key], expected[key], "lost database field: {key}");
        }
        let cas = PayloadCas::new(directory.path()).unwrap();
        for (key, payload, kind, mime) in [
            ("picture", "synthetic picture", "image", "image/webp"),
            ("voice", "synthetic voice", "audio", "audio/mpeg"),
            ("movie", "synthetic movie", "video", "video/mp4"),
            (
                "signature",
                "{\"strokes\":[]}",
                "signature",
                "application/json",
            ),
        ] {
            let alias = store
                .read_asset_alias("inlay", key, None)
                .unwrap()
                .unwrap()
                .value;
            assert_eq!(alias.inlay_type.as_deref(), Some(kind));
            assert_eq!(alias.mime, mime);
            if key == "picture" {
                assert_eq!(alias.metadata["pocketRisu"]["createdAt"], 123);
                assert_eq!(alias.metadata["pocketRisu"]["chatId"], "synthetic-chat");
            }
            assert_eq!(
                std::fs::read(
                    cas.object_path(alias.object_hash.as_deref().unwrap())
                        .unwrap()
                        .unwrap()
                )
                .unwrap(),
                payload.as_bytes()
            );
        }
        assert!(store
            .read_asset_alias("asset", "assets/inlay_meta/picture", None)
            .unwrap()
            .is_none());
        assert!(store
            .read_asset_alias("asset", "assets/module.png", None)
            .unwrap()
            .is_some());
        assert_eq!(store.revision().unwrap(), 2);
    }
}

#[test]
fn invalid_database_never_activates_prepared_pocket_media() {
    let directory = tempfile::tempdir().unwrap();
    seed(directory.path());
    let job = JobRegistry::default()
        .create(JobKind::RestoreLegacyLocalBackup)
        .unwrap();
    job.start(JobPhase::ReadingSource).unwrap();
    assert!(parse_legacy_local_backup_v1(
        &mut Cursor::new(V112),
        directory.path(),
        PayloadTarget::JobStaging,
        &mut Import {
            root: directory.path(),
            job: &job,
            fail_database: true
        },
        &NeverCancelled
    )
    .is_err());
    let store = PersistentStore::open(directory.path()).unwrap();
    assert_eq!(store.revision().unwrap(), 1);
    assert_eq!(
        store.read_root(None).unwrap().value["username"],
        "Existing data"
    );
    assert!(store
        .read_asset_alias("inlay", "picture", None)
        .unwrap()
        .is_none());
}

fn archive_entry(name: &str, data: &[u8]) -> Vec<u8> {
    [
        (name.len() as u32).to_le_bytes().as_slice(),
        name.as_bytes(),
        &(data.len() as u32).to_le_bytes(),
        data,
    ]
    .concat()
}

fn plan(
    entries: Vec<u8>,
    cancellation: &dyn CancellationProbe,
) -> Result<PreparedLegacyRestorePayloads, LocalBackupError> {
    let directory = tempfile::tempdir().unwrap();
    let cas = PayloadCas::new(directory.path()).unwrap();
    struct Planner<'a>(
        &'a PayloadCas,
        &'a mut DurableCasJob,
        &'a dyn CancellationProbe,
        Option<PreparedLegacyRestorePayloads>,
    );
    impl StrictLocalBackupDatabaseRestore for Planner<'_> {
        fn restore_database(
            &mut self,
            _: &StagedLocalBackupEntry,
            entries: &[StagedLocalBackupEntry],
        ) -> Result<(), LocalBackupError> {
            self.3 = Some(prepare_legacy_restore_payloads(
                entries, self.0, self.1, self.2,
            )?);
            Ok(())
        }
    }
    let mut durable = DurableCasJob::begin(
        directory.path(),
        &Uuid::new_v4().to_string(),
        CasJobKind::LocalBackupRestore,
        now_millis(),
    )
    .unwrap();
    let mut planner = Planner(&cas, &mut durable, cancellation, None);
    let outcome = parse_legacy_local_backup_v1(
        &mut Cursor::new([entries, archive_entry(DATABASE_ENTRY, b"RISUSAVE\0")].concat()),
        directory.path(),
        PayloadTarget::JobStaging,
        &mut planner,
        &NeverCancelled,
    )
    .map(|_| planner.3.take().unwrap());
    durable.release(CasReleaseOutcome::Aborted).unwrap();
    outcome
}

#[test]
fn pairs_early_sidecars_and_infers_missing_metadata() {
    let payloads = plan(
        [
            archive_entry(
                "inlay_sidecar/a",
                br#"{"ext":".WEBP","name":"original","type":"image"}"#,
            ),
            archive_entry("inlay/a.webp", b"a"),
            archive_entry("inlay/b.MP3", b"b"),
        ]
        .concat(),
        &NeverCancelled,
    )
    .unwrap();
    assert_eq!(payloads.asset_aliases[0].name, "original");
    assert_eq!(payloads.asset_aliases[0].ext, "webp");
    assert_eq!(payloads.asset_aliases[1].mime, "audio/mpeg");
}

#[test]
fn rejects_corrupt_metadata_duplicates_unsafe_paths_and_truncated_payloads() {
    for entries in [
        [
            archive_entry("inlay/a.webp", b"a"),
            archive_entry("inlay_sidecar/a", b"bad JSON"),
        ]
        .concat(),
        [
            archive_entry("inlay/a.webp", b"a"),
            archive_entry(
                "inlay_sidecar/a",
                br#"{"ext":"webp","name":"a","type":"invalid"}"#,
            ),
        ]
        .concat(),
        [
            archive_entry("inlay/a.webp", b"a"),
            archive_entry("inlay/a.png", b"b"),
        ]
        .concat(),
        [
            archive_entry("inlay_sidecar/a", b"{}"),
            archive_entry("inlay_info/a", b"{}"),
        ]
        .concat(),
        archive_entry("inlay/nested/a.webp", b"a"),
        archive_entry("inlay/a.unknown", b"a"),
        archive_entry("inlay/a.webp", b"a")[..19].to_vec(),
        [
            archive_entry("inlay/a.webp", b"a"),
            archive_entry(
                "inlay_sidecar/a",
                &vec![b' '; MAX_METADATA_BYTES as usize + 1],
            ),
        ]
        .concat(),
        [
            archive_entry("inlay/a.webp", b"a"),
            archive_entry(
                "inlay_sidecar/a",
                br#"{"ext":"webp","name":"a","type":"image","width":-1}"#,
            ),
        ]
        .concat(),
    ] {
        assert!(plan(entries, &NeverCancelled).is_err());
    }
}

#[test]
fn cancels_before_preparing_pocket_payloads() {
    struct Cancel;
    impl CancellationProbe for Cancel {
        fn is_cancelled(&self) -> bool {
            true
        }
    }
    assert_eq!(
        plan(archive_entry("inlay/a.webp", b"a"), &Cancel)
            .err()
            .unwrap()
            .code,
        LocalBackupErrorCode::Cancelled
    );
}

#[test]
#[ignore = "streams more than 1 GiB through staging and CAS; run explicitly for large-import validation"]
fn pocket_backup_over_one_gib_restores_without_archive_sized_buffers() {
    let directory = tempfile::tempdir().unwrap();
    seed(directory.path());
    let length = 1024_u64 * 1024 * 1024 + 65536;
    let path = directory.path().join("large-source.bin");
    let mut file = File::create(&path).unwrap();
    let name = b"inlay/large.mp4";
    file.write_all(&(name.len() as u32).to_le_bytes()).unwrap();
    file.write_all(name).unwrap();
    file.write_all(&(length as u32).to_le_bytes()).unwrap();
    file.seek(SeekFrom::Current(length as i64)).unwrap();
    file.write_all(V112).unwrap();
    drop(file);

    struct BoundedSource {
        file: File,
        largest_read: usize,
    }
    impl Read for BoundedSource {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            assert!(bytes.len() <= 64 * 1024, "archive-sized read requested");
            self.largest_read = self.largest_read.max(bytes.len());
            self.file.read(bytes)
        }
    }
    let mut source = BoundedSource {
        file: File::open(&path).unwrap(),
        largest_read: 0,
    };
    let job = JobRegistry::default()
        .create(JobKind::RestoreLegacyLocalBackup)
        .unwrap();
    job.start(JobPhase::ReadingSource).unwrap();
    let report = parse_legacy_local_backup_v1(
        &mut source,
        directory.path(),
        PayloadTarget::JobStaging,
        &mut Import {
            root: directory.path(),
            job: &job,
            fail_database: false,
        },
        &NeverCancelled,
    )
    .unwrap();
    let store = PersistentStore::open(directory.path()).unwrap();
    let alias = store
        .read_asset_alias("inlay", "large", None)
        .unwrap()
        .unwrap()
        .value;
    assert_eq!(alias.size as u64, length);
    assert_eq!(alias.mime, "video/mp4");
    assert_eq!(store.revision().unwrap(), 2);
    assert_eq!(report.source_bytes, std::fs::metadata(path).unwrap().len());
    assert_eq!(
        PayloadCas::new(directory.path())
            .unwrap()
            .stat_object(alias.object_hash.as_deref().unwrap())
            .unwrap(),
        Some(length)
    );
    println!(
        "Restored {} archive bytes; largest source read = {} bytes",
        report.source_bytes, source.largest_read
    );
}
