use super::*;
use crate::local_backup::{parse_legacy_local_backup_v1, NeverCancelled};
use crate::native_file_jobs::{JobKind, JobRegistry};
use std::io::{Cursor, Seek, SeekFrom};

const V110: &[u8] = include_bytes!("fixtures/pocket-risu-v1.10.0.bin");
const V112: &[u8] = include_bytes!("fixtures/pocket-risu-v1.12.0.bin");
const V114: &[u8] = include_bytes!("fixtures/pocket-risu-v1.14.0.bin");

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
        self.store.lock().unwrap().replace_put_upstream_root(id, root)
    }
    fn put_presets(&self, id: &str, presets: &[Value]) -> StoreResult<()> {
        self.store.lock().unwrap().replace_put_upstream_presets(id, presets)
    }
    fn put_legacy_root(&self, id: &str, root: &crate::persistent_store::upstream_stream::RootSpool) -> StoreResult<()> {
        self.store.lock().unwrap().replace_put_upstream_stream(id, root)
    }
    fn add_characters(&self, id: &str, characters: &[Value]) -> StoreResult<()> {
        let mut unavailable = Vec::new();
        let expanded =
            cold_expansion::expand_cold_payloads(characters, &self.payloads.cold_payloads, &mut unavailable)
                .map_err(|message| crate::persistent_store::StoreError::Store { message })?;
        assert!(unavailable.is_empty(), "synthetic PocketRisu fixture has unavailable cold payloads");
        self.store
            .lock()
            .unwrap()
            .replace_add_characters(id, expanded.as_deref().unwrap_or(characters))
    }
    fn commit(&self, id: &str, revision: i64, _binding_authority: Option<&str>) -> StoreResult<RevisionResult> {
        let mut store = self.store.lock().unwrap();
        store.replace_put_asset_aliases(id, &self.payloads.asset_aliases)?;
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
            crate::asset_repository::job_pins::CasJobOwner::for_test(),
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
        let result = restore::restore_started_risu_save(
            OpenedJobSource {
                file: open_staged(database)?,
                total_bytes: database.byte_length,
                custody: None,
            },
            1,
            self.job,
            &sink,
            restore::RestoreProgressScale { spool_directory: Some(self.root.to_path_buf()), ..Default::default() },
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
fn pocket_versions_restore_database_and_media_into_the_persistent_store() {
    for archive in [V110, V112, V114] {
        let directory = tempfile::tempdir().unwrap();
        seed(directory.path());
        let job = JobRegistry::default()
            .create(JobKind::RestoreLegacyLocalBackup)
            .unwrap();
        job.start(JobPhase::ReadingSource).unwrap();
        parse_legacy_local_backup_v1(
            &mut Cursor::new(archive),
            &test_legacy_staging(directory.path()),
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
        let mut expected: Value = serde_json::from_str(include_str!("fixtures/expected.json")).unwrap();
        let imported_username = expected["username"].clone();
        expected["personas"][0]["name"] = imported_username;
        expected["selectedPersona"] = expected["personas"][0]["id"].clone();
        expected["botPresetsId"] = expected["botPresets"][0]["id"].clone();
        let archived = expected["characters"].as_array_mut().unwrap().remove(1);
        expected["characters"].as_array_mut().unwrap().push(archived);
        for (key, expected_value) in expected.as_object().unwrap() {
            assert_eq!(&actual[key], expected_value, "lost database field: {key}");
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
        &test_legacy_staging(directory.path()),
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

// Local diagnostic only: never print archive values, internal names, or raw errors.
#[test]
#[ignore = "requires RISUNEST_DIAGNOSTIC_BACKUP; imports into a disposable isolated store"]
fn private_backup_diagnostic() {
    let source = std::env::var_os("RISUNEST_DIAGNOSTIC_BACKUP").expect("source not configured");
    let mut source = File::open(source).expect("cannot open diagnostic source");
    let directory = tempfile::tempdir().unwrap();
    seed(directory.path());
    let job = JobRegistry::default()
        .create(JobKind::RestoreLegacyLocalBackup)
        .unwrap();
    job.start(JobPhase::ReadingSource).unwrap();
    let result = parse_legacy_local_backup_v1(
        &mut source,
        &test_legacy_staging(directory.path()),
        PayloadTarget::JobStaging,
        &mut Import {
            root: directory.path(),
            job: &job,
            fail_database: false,
        },
        &NeverCancelled,
    );
    let succeeded = result.is_ok();
    match result {
        Ok(_) => println!("PRIVATE_DIAGNOSTIC: native import succeeded in disposable store"),
        Err(error) => {
            let safe_reasons = [
                "requires unique, nonempty chat IDs",
                "requires a nonempty character ID",
                "requires unique character IDs",
                "PocketRisu Inlay metadata is invalid",
                "PocketRisu Inlay entry must have a single filename",
                "PocketRisu Inlay needs metadata for this extension",
                "binary values are unsupported in a legacy MessagePack database",
                "unsupported legacy MessagePack extension",
                "legacy backup cold payload is invalid",
                "decoded legacy RisuSave limit exceeded",
                "invalid legacy MessagePack",
                "truncated legacy backup entry body",
                "legacy backup does not contain database.risudat",
            ];
            let reason = safe_reasons
                .iter()
                .find(|reason| error.message.contains(**reason))
                .copied()
                .unwrap_or("unclassified error (details withheld)");
            println!(
                "PRIVATE_DIAGNOSTIC: code={:?}; phase={:?}; reason={reason}",
                error.code,
                job.status().phase
            );
        }
    }
    assert!(
        succeeded,
        "PRIVATE_DIAGNOSTIC: native import failed (details withheld)"
    );
}

#[test]
fn pocket_namespaced_cold_storage_is_staged_for_expansion() {
    for key in ["9f8b7c6d-1a2b-3c4d-5e6f-a1b2c3d4e5f6", "custom-cold-key"] {
        let result = plan(
            archive_entry(&format!("coldstorage/{key}.json"), br#"[{"message":[]}]"#),
            &NeverCancelled,
        )
        .unwrap();
        assert_eq!(result.cold_payloads.len(), 1);
        assert!(result.cold_payloads.contains_key(key));
        assert!(result.asset_aliases.is_empty());
    }
}

#[test]
fn duplicate_cold_namespaces_cannot_activate_and_invalid_cold_data_is_unavailable() {
    let key = "9f8b7c6d-1a2b-3c4d-5e6f-a1b2c3d4e5f6";
    let duplicates = [
        archive_entry(&format!("coldstorage/{key}.json"), b"[]"),
        archive_entry(&format!("coldstorage_{key}.json"), b"[]"),
    ]
    .concat();
    assert!(plan(duplicates, &NeverCancelled).is_err());
    let invalid = plan(
        archive_entry(&format!("coldstorage/{key}.json"), b"invalid"),
        &NeverCancelled,
    )
    .unwrap();
    assert!(invalid.cold_payloads.is_empty());
    assert_eq!(
        invalid.unavailable_cold_payloads.get(key),
        Some(&restore::ColdPayloadUnavailable::Damaged)
    );
}

fn plan(
    entries: Vec<u8>,
    cancellation: &dyn CancellationProbe,
) -> Result<PreparedLegacyRestorePayloads, LocalBackupError> {
    let directory = tempfile::tempdir().unwrap();
    drop(PersistentStore::open(directory.path()).unwrap());
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
        crate::asset_repository::job_pins::CasJobOwner::for_test(),
        now_millis(),
    )
    .unwrap();
    let mut planner = Planner(&cas, &mut durable, cancellation, None);
    let outcome = parse_legacy_local_backup_v1(
        &mut Cursor::new([entries, archive_entry(DATABASE_ENTRY, b"RISUSAVE\0")].concat()),
        &test_legacy_staging(directory.path()),
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
    for metadata_kind in ["inlay_sidecar", "inlay_info"] {
        for (metadata, expected_ext) in [
            (br#"{}"#.as_slice(), "bin"),
            (br#"{"ext":" "}"#.as_slice(), "bin"),
            (br#"{"ext":42}"#.as_slice(), "bin"),
            (br#"{"ext":"bin"}"#.as_slice(), "bin"),
        ] {
            let payloads = plan([
                archive_entry(&format!("{metadata_kind}/movie"), metadata),
                archive_entry("inlay/movie.MP4", b"synthetic video bytes"),
            ].concat(), &NeverCancelled).unwrap();
            assert_eq!(payloads.asset_aliases.len(), 1, "Known raw filename must remain readable");
            let alias = &payloads.asset_aliases[0];
            assert_eq!(alias.ext, expected_ext);
            assert_eq!(alias.mime, "video/mp4");
            assert_eq!(alias.name, "movie");
            assert_eq!(alias.inlay_type.as_deref(), Some("image"));
            assert_eq!(alias.size, 21);
        }
        let unknown = plan([
            archive_entry(&format!("{metadata_kind}/movie"), br#"{"ext":"unknown"}"#),
            archive_entry("inlay/movie.mp4", b"unsupported bytes"),
        ].concat(), &NeverCancelled).unwrap();
        assert!(unknown.asset_aliases.is_empty());
    }
}

#[test]
fn rejects_duplicate_metadata_and_truncated_or_oversized_payloads() {
    for entries in [
        [
            archive_entry("inlay/a.webp", b"a"),
            archive_entry("inlay/a.png", b"b"),
        ]
        .concat(),
        [
            archive_entry("inlay_sidecar/a", b"{}"),
            archive_entry("inlay_sidecar/a", b"{}"),
        ]
        .concat(),
        archive_entry("inlay/a.webp", b"a")[..19].to_vec(),
        [
            archive_entry("inlay/a.webp", b"a"),
            archive_entry(
                "inlay_sidecar/a",
                &vec![b' '; MAX_METADATA_BYTES as usize + 1],
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
fn unsupported_inlay_shapes_are_counted_without_losing_readable_payloads() {
    let payloads = plan([
        archive_entry("inlay/good.webp", b"kept"),
        archive_entry("inlay/a.unknown", b"uninterpreted"),
        archive_entry("inlay/nested/a.webp", b"uninterpreted"),
        archive_entry("inlay/b.webp", b"uninterpreted"),
        archive_entry("inlay_sidecar/b", br#"{"ext":"webp","name":"b","type":"future"}"#),
        archive_entry("inlay/broken-json", b"{\"data\":"),
        archive_entry("inlay/broken-data-type", br#"{"ext":"png","name":"Broken","type":"image","data":42}"#),
        archive_entry("inlay/broken-sidecar.webp", b"ignored"),
        archive_entry("inlay_sidecar/broken-sidecar", b"bad JSON"),
        archive_entry("inlay/broken-provenance.webp", b"ignored"),
        archive_entry("inlay_meta/broken-provenance", b"bad JSON"),
    ].concat(), &NeverCancelled).unwrap();
    assert_eq!(payloads.asset_aliases.len(), 1);
    assert_eq!(payloads.asset_aliases[0].key, "good");
    assert_eq!(payloads.invalid_inlays, ["inlay/b.webp", "inlay/broken-json", "inlay/broken-data-type", "inlay/broken-sidecar.webp", "inlay/broken-provenance.webp"]);
    assert_eq!(payloads.skipped_inlays, 2);
}

#[test]
fn extensionless_inlays_preserve_base64_signature_and_raw_sidecar_bytes() {
    use sha2::{Digest, Sha256};
    let raw = b"{raw signature bytes";
    let payloads = plan([
        archive_entry("inlay/base64", br#"{"ext":"png","name":"Image","type":"image","data":"AAEC//7+"}"#),
        archive_entry("inlay/url", br#"{"data":"data:image/png;base64,AAEC//7+","ext":"png","name":"URL","type":"image"}"#),
        archive_entry("inlay/signature", br#"{"type":"signature","data":"\uD83D\uDE00\ud800x\udc00\n"}"#),
        archive_entry("inlay/raw", raw),
        archive_entry("inlay_sidecar/raw", br#"{"ext":"json","name":"Raw","type":"signature"}"#),
        archive_entry("inlay/duplicate", br#"{"data":"longer ignored string","ext":"json","name":"Last","type":"signature","data":"last"}"#),
        archive_entry("inlay/defaults", br#"{"data":"iVBORw0KGgo="}"#),
        archive_entry("inlay/no-type", br#"{"ext":"png","name":"Named","data":"AAEC"}"#),
        archive_entry("inlay/no-name", br#"{"ext":"png","type":"image","data":"AAEC"}"#),
        archive_entry("inlay/precedence", br#"{"ext":"png","type":"image","name":"Own","data":"AAEC"}"#),
        archive_entry("inlay_info/precedence", br#"{"ext":"gif","type":"image","name":"Legacy"}"#),
        archive_entry("inlay_sidecar/precedence", br#"{"ext":"  ..W/E\\BP\u0000  ","type":"image","name":"Sidecar"}"#),
        archive_entry("inlay/legacy-info", br#"{"ext":"png","type":"image","name":"Own","data":"AAEC"}"#),
        archive_entry("inlay_info/legacy-info", br#"{"ext":"gif","name":"Legacy"}"#),
        archive_entry("inlay/raw-png", b"\x89PNG\r\n\x1a\n"),
        archive_entry("inlay/trailing-dot.", br#"{"data":"iVBORw0KGgo="}"#),
        archive_entry("inlay/raw-jpeg.weird", &[0xff, 0xd8, 0xff, 0]),
        archive_entry("inlay/raw-webp", b"RIFF1234WEBP"),
        archive_entry("inlay/raw-gif", b"GIF89a"),
        archive_entry("inlay/raw-avif", b"\0\0\0\x18ftypmif1\0\0\0\0mif1avif"),
        archive_entry("inlay/unrecognized", b"unrecognized synthetic data"),
    ].concat(), &NeverCancelled).unwrap();
    assert!(payloads.invalid_inlays.is_empty());
    assert_eq!(payloads.skipped_inlays, 1);
    let signature = payloads.asset_aliases.iter().find(|alias| alias.key == "signature").unwrap();
    assert_eq!(signature.ext, "bin");
    assert_eq!(signature.name, "signature");
    for (id, name, ext) in [("defaults", "defaults", "png"), ("no-type", "Named", "png"), ("no-name", "no-name", "png"), ("precedence", "Sidecar", "webp"), ("legacy-info", "Legacy", "gif"), ("raw-png", "raw-png", "png"), ("trailing-dot", "trailing-dot", "png"), ("raw-jpeg", "raw-jpeg", "jpg"), ("raw-webp", "raw-webp", "webp"), ("raw-gif", "raw-gif", "gif"), ("raw-avif", "raw-avif", "avif")] {
        let alias = payloads.asset_aliases.iter().find(|alias| alias.key == id).unwrap();
        assert_eq!(alias.name, name, "{id}");
        assert_eq!(alias.ext, ext, "{id}");
        assert_eq!(alias.inlay_type.as_deref(), Some("image"), "{id}");
    }
    for (id, bytes) in [
        ("base64", &[0, 1, 2, 255, 254, 254][..]),
        ("url", &[0, 1, 2, 255, 254, 254][..]),
        ("signature", "😀�x�\n".as_bytes()), ("raw", raw.as_slice()), ("duplicate", b"last".as_slice()),
    ] {
        let alias = payloads.asset_aliases.iter().find(|alias| alias.key == id).unwrap();
        assert_eq!(alias.object_hash.as_deref(), Some(hex::encode(Sha256::digest(bytes)).as_str()), "{id}");
        assert_eq!(alias.size, bytes.len() as i64, "{id}");
    }
    for metadata_kind in ["inlay_sidecar", "inlay_info"] {
        for (metadata, ext, kind, mime, name, dimensions) in [
            (serde_json::json!({"type":"audio","name":"External","width":2}), "mp3", "audio", "audio/mpeg", "External", (Some(2), None)),
            (serde_json::json!({"ext":"mp3"}), "mp3", "audio", "audio/mpeg", "voice", (None, None)),
            (serde_json::json!({}), "mp3", "audio", "audio/mpeg", "voice", (None, None)),
            (serde_json::json!({"ext":"mp4","type":"video","name":"External","width":2,"height":3}), "mp4", "video", "video/mp4", "External", (Some(2), Some(3))),
            (serde_json::json!({"ext":"mp3","type":null}), "mp3", "image", "audio/mpeg", "voice", (None, None)),
        ] {
            let mut entries = vec![
                archive_entry("inlay/voice", br#"{"ext":"mp3","type":"audio","name":"Own","width":10,"height":20,"data":"AAEC"}"#),
                archive_entry(&format!("{metadata_kind}/voice"), &serde_json::to_vec(&metadata).unwrap()),
            ];
            if metadata_kind == "inlay_sidecar" {
                entries.push(archive_entry("inlay_info/voice", br#"{"ext":"gif","type":"image","name":"Legacy","width":7,"height":8}"#));
            }
            let payloads = plan(entries.concat(), &NeverCancelled).unwrap();
            assert!(payloads.invalid_inlays.is_empty());
            assert_eq!(payloads.skipped_inlays, 0);
            assert_eq!(payloads.asset_aliases.len(), 1);
            let alias = &payloads.asset_aliases[0];
            assert_eq!(alias.ext, ext, "{metadata_kind}: {metadata}");
            assert_eq!(alias.inlay_type.as_deref(), Some(kind), "{metadata_kind}: {metadata}");
            assert_eq!(alias.mime, mime);
            assert_eq!(alias.name, name);
            assert_eq!((alias.width, alias.height), dimensions);
            assert_eq!(alias.object_hash.as_deref(), Some(hex::encode(Sha256::digest([0, 1, 2])).as_str()));
            assert_eq!(alias.size, 3);
        }
        let unsupported = plan([
            archive_entry("inlay/voice", br#"{"ext":"mp3","type":"audio","data":"AAEC"}"#),
            archive_entry(&format!("{metadata_kind}/voice"), br#"{"ext":"unknown","type":"audio"}"#),
        ].concat(), &NeverCancelled).unwrap();
        assert!(unsupported.asset_aliases.is_empty());
        assert_eq!(unsupported.skipped_inlays, 1);
    }
}

#[test]
fn pocket_backup_payload_restores_through_source_reads_of_at_most_64_kib() {
    let directory = tempfile::tempdir().unwrap();
    seed(directory.path());
    let length = 4_u64 * 1024 * 1024 + 65537;
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
        &test_legacy_staging(directory.path()),
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
    assert!(source.largest_read > 0);
}
