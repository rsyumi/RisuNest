use super::{JobControl, JobPhase, JobProgress, JobResultSummary, NativeJobError, OpenedJobSource};
use crate::asset_repository::job_pins::{
    CasJobKind, CasObjectRole, CasReleaseOutcome, DurableCasJob,
};
use crate::asset_repository::PayloadCas;
use crate::import_export_jobs::{classify_content, ContentKind};
use crate::persistent_store::{AssetAlias, AssetOwnerLocator, PersistentStore, WorkingSetCommit};
use serde_json::{json, Value};
use std::io::{Read, Seek, SeekFrom};
use std::time::{SystemTime, UNIX_EPOCH};

const COPY_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub(crate) enum JpegAssetDestination {
    CurrentCharacterImage { character_id: String },
}

impl JpegAssetDestination {
    fn character_id(&self) -> &str {
        match self {
            Self::CurrentCharacterImage { character_id } => character_id,
        }
    }
}

struct ValidatedDestination {
    pub(super) character_id: String,
}

struct CancellableProgressReader<'a> {
    source: &'a mut std::fs::File,
    job: &'a JobControl,
    completed_bytes: u64,
    total_bytes: u64,
}

impl Read for CancellableProgressReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.job.is_cancel_requested() {
            return Err(std::io::Error::other(
                "native JPEG asset import was cancelled",
            ));
        }
        let limit = buffer.len().min(COPY_BUFFER_BYTES);
        let read = self.source.read(&mut buffer[..limit])?;
        self.completed_bytes = self
            .completed_bytes
            .checked_add(read as u64)
            .ok_or_else(|| std::io::Error::other("JPEG source size overflow"))?;
        self.job
            .set_progress(JobProgress {
                completed_bytes: self.completed_bytes,
                total_bytes: Some(self.total_bytes),
                completed_items: 0,
                total_items: Some(1),
            })
            .map_err(std::io::Error::other)?;
        Ok(read)
    }
}

fn now_millis() -> Result<i64, NativeJobError> {
    let value = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| NativeJobError::new("store-error", error.to_string()))?
        .as_millis();
    i64::try_from(value)
        .map_err(|_| NativeJobError::new("store-error", "system time is out of range"))
}

fn job_state_error(job: &JobControl, error: String) -> NativeJobError {
    if job.is_cancel_requested() {
        NativeJobError::new("cancelled", "native JPEG asset import was cancelled")
    } else {
        NativeJobError::new("store-error", error)
    }
}

fn jpeg_extension(display_name: &str) -> Result<&'static str, NativeJobError> {
    match display_name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg") => Ok("jpg"),
        Some("jpeg") => Ok("jpeg"),
        _ => Err(NativeJobError::new(
            "invalid-input",
            "native JPEG asset import requires a .jpg or .jpeg display name",
        )),
    }
}

fn validate_ordinary_jpeg(
    source: &mut OpenedJobSource,
    display_name: &str,
    job: &JobControl,
) -> Result<(), NativeJobError> {
    let kind = classify_content(
        display_name,
        &mut source.file,
        &super::content::content_classification_limits(),
        &|| job.is_cancel_requested(),
    )
    .map_err(super::content::native_format_error)?;
    source
        .file
        .seek(SeekFrom::Start(0))
        .map_err(|error| NativeJobError::new("store-error", error.to_string()))?;
    let mut signature = [0_u8; 3];
    source
        .file
        .read_exact(&mut signature)
        .map_err(|error| NativeJobError::new("invalid-input", error.to_string()))?;
    source
        .file
        .seek(SeekFrom::Start(0))
        .map_err(|error| NativeJobError::new("store-error", error.to_string()))?;
    if kind != ContentKind::JpegAsset || signature != [0xff, 0xd8, 0xff] {
        return Err(NativeJobError::new(
            "invalid-input",
            "native JPEG asset import requires an ordinary JPEG source",
        ));
    }
    Ok(())
}

fn character_image_extension(uri: &str) -> &'static str {
    let path = uri
        .split(|character| matches!(character, '?' | '#'))
        .next()
        .unwrap_or(uri);
    let extension = path
        .rsplit_once('.')
        .map_or("", |(_, extension)| extension)
        .to_ascii_lowercase();
    match extension.as_str() {
        "png" => "png",
        "webp" => "webp",
        "gif" => "gif",
        "jpg" => "jpg",
        "jpeg" => "jpeg",
        _ => "png",
    }
}

fn update_character_image(
    mut character: Value,
    character_id: &str,
    logical_id: &str,
) -> Result<Value, NativeJobError> {
    let object = character.as_object_mut().ok_or_else(|| {
        NativeJobError::new("invalid-destination", "destination character is invalid")
    })?;
    if object.get("chaId").and_then(Value::as_str) != Some(character_id) {
        return Err(NativeJobError::new(
            "invalid-destination",
            "destination character identity does not match",
        ));
    }
    if let Some(previous) = object
        .get("image")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
    {
        let extension = character_image_extension(&previous);
        let assets = object
            .entry("ccAssets")
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| {
                NativeJobError::new(
                    "invalid-destination",
                    "destination character ccAssets is invalid",
                )
            })?;
        assets.push(json!({
            "type": "icon",
            "name": "iconx",
            "uri": previous,
            "ext": extension,
        }));
    }
    object.insert("image".to_owned(), Value::String(logical_id.to_owned()));
    Ok(character)
}

pub(super) fn import_jpeg_asset(
    source: OpenedJobSource,
    display_name: &str,
    destination: JpegAssetDestination,
    expected_revision: i64,
    repository_root: &std::path::Path,
    store: PersistentStore,
    job: &JobControl,
) -> Result<JobResultSummary, NativeJobError> {
    import_jpeg_asset_with_before_commit(
        source,
        display_name,
        destination,
        expected_revision,
        repository_root,
        store,
        job,
        || Ok(()),
    )
}

fn import_jpeg_asset_with_before_commit(
    mut source: OpenedJobSource,
    display_name: &str,
    destination: JpegAssetDestination,
    expected_revision: i64,
    repository_root: &std::path::Path,
    mut store: PersistentStore,
    job: &JobControl,
    before_commit: impl FnOnce() -> Result<(), NativeJobError>,
) -> Result<JobResultSummary, NativeJobError> {
    job.start(JobPhase::ReadingSource)
        .map_err(|error| job_state_error(job, error))?;
    job.set_progress(JobProgress {
        completed_bytes: 0,
        total_bytes: Some(source.total_bytes),
        completed_items: 0,
        total_items: Some(1),
    })
    .map_err(|error| job_state_error(job, error))?;

    let destination = ValidatedDestination {
        character_id: destination.character_id().to_owned(),
    };
    if destination.character_id.is_empty() || destination.character_id.len() > 256 {
        return Err(NativeJobError::new(
            "invalid-destination",
            "native JPEG asset destination character is invalid",
        ));
    }
    let extension = jpeg_extension(display_name)?;
    validate_ordinary_jpeg(&mut source, display_name, job)?;
    if store.revision().map_err(super::native_store_error)? != expected_revision {
        return Err(NativeJobError::new(
            "revision-conflict",
            "native JPEG asset destination revision changed",
        ));
    }
    let versioned_character = store
        .read_character(&destination.character_id, None)
        .map_err(super::native_store_error)?
        .ok_or_else(|| {
            NativeJobError::new(
                "invalid-destination",
                "native JPEG asset destination character does not exist",
            )
        })?;
    let owner = AssetOwnerLocator::CharacterAdditionalAssets {
        character_id: destination.character_id.clone(),
    };
    let owner_head = store
        .read_asset_owner_head(&owner, None)
        .map_err(super::native_store_error)?
        .map(|versioned| versioned.value);

    let cas = PayloadCas::new(repository_root)
        .map_err(|error| NativeJobError::new("store-error", error.to_string()))?;
    let mut cas_job = DurableCasJob::begin(
        repository_root,
        &job.id(),
        CasJobKind::DirectAssetOrInlayWrite,
        now_millis()?,
    )
    .map_err(|error| NativeJobError::new("store-error", error.to_string()))?;
    let outcome = (|| {
        let mut reader = CancellableProgressReader {
            source: &mut source.file,
            job,
            completed_bytes: 0,
            total_bytes: source.total_bytes,
        };
        let prepared = cas_job
            .prepare_reader(&cas, &mut reader, CasObjectRole::DirectObject)
            .map_err(|error| {
                if job.is_cancel_requested() {
                    NativeJobError::new("cancelled", "native JPEG asset import was cancelled")
                } else {
                    NativeJobError::new("store-error", error.to_string())
                }
            })?;
        if prepared.byte_size != source.total_bytes {
            return Err(NativeJobError::new(
                "invalid-input",
                "native JPEG asset source length changed while reading",
            ));
        }
        if job.is_cancel_requested() {
            return Err(NativeJobError::new(
                "cancelled",
                "native JPEG asset import was cancelled",
            ));
        }
        let logical_id = format!("assets/{}.{}", prepared.content_hash, extension);
        let character = update_character_image(
            versioned_character.value,
            &destination.character_id,
            &logical_id,
        )?;
        let alias = AssetAlias {
            key: logical_id,
            object_hash: Some(prepared.content_hash.clone()),
            kind: "asset".to_owned(),
            size: i64::try_from(prepared.byte_size).map_err(|_| {
                NativeJobError::new("native-limit", "JPEG asset size exceeds storage limits")
            })?,
            mime: "image/jpeg".to_owned(),
            name: display_name.to_owned(),
            ext: extension.to_owned(),
            inlay_type: None,
            width: None,
            height: None,
            metadata: json!({}),
        };
        cas_job
            .seal(&mut store, now_millis()?)
            .map_err(|error| NativeJobError::new("store-error", error.to_string()))?;
        job.set_phase(JobPhase::ActivatingDatabase)
            .map_err(|error| job_state_error(job, error))?;
        before_commit()?;
        let committed = store
            .commit_with_asset_aliases(
                &WorkingSetCommit {
                    expected_revision,
                    root_mutations: None,
                    root: None,
                    replace_presets: None,
                    character: Some(character),
                    character_details: None,
                    replace_character: None,
                    add_character: None,
                    conversations: None,
                    delete_character_id: None,
                    plugin_storage: None,
                    asset_owner_heads: owner_head.map(|head| vec![head]),
                },
                &[alias],
            )
            .map_err(super::native_store_error)?;
        Ok(JobResultSummary {
            export_exclusions: None,
            revision: committed.revision,
            source_bytes: prepared.byte_size,
            source_sha256: prepared.content_hash,
            character_count: 1,
            preset_count: 0,
            warning_codes: Vec::new(),
            handoff_path: None,
            publication: None,
        })
    })();

    match outcome {
        Ok(mut result) => {
            if cas_job.release(CasReleaseOutcome::Committed).is_err() {
                result.warning_codes.push("cleanup-failed".to_owned());
            }
            Ok(result)
        }
        Err(error) => match cas_job.release(CasReleaseOutcome::Aborted) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(NativeJobError::new(
                "cleanup-failed",
                format!("{}; cleanup failed: {cleanup}", error.message),
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::super::{JobKind, JobRegistry};
    use super::*;
    use crate::asset_repository::job_pins::collect_durable_cas_job_roots;
    use crate::asset_repository::owner_manifest_codec::{
        encode_owner_manifest, OwnerManifestEntry,
    };
    use crate::persistent_store::{AssetOwnerHead, PersistentStore};
    use serde_json::Value;
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::io::{Cursor, Read, Write};
    use zip::write::FileOptions;
    use zip::{CompressionMethod, ZipWriter};

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../../fixtures/persistent-fixture.json"))
            .expect("parse persistent fixture")
    }

    #[test]
    fn cancellation_reader_uses_a_non_retryable_error_for_exact_reads() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("synthetic.jpg");
        fs::write(&path, [1_u8]).unwrap();
        let mut source = fs::File::open(path).unwrap();
        let job = JobRegistry::default()
            .create(JobKind::ImportJpegAsset)
            .unwrap();
        job.request_cancel().unwrap();
        let mut reader = CancellableProgressReader {
            source: &mut source,
            job: &job,
            completed_bytes: 0,
            total_bytes: 1,
        };
        let error = reader.read_exact(&mut [0_u8; 1]).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
    }

    fn open_fixture() -> (tempfile::TempDir, PersistentStore, String, AssetOwnerHead) {
        let directory = tempfile::tempdir().expect("create temp directory");
        let mut store = PersistentStore::open(directory.path()).expect("open persistent store");
        let database = fixture();
        let mut characters = database["characters"]
            .as_array()
            .expect("fixture characters")
            .clone();
        let character_id = characters[0]["chaId"]
            .as_str()
            .expect("fixture character ID")
            .to_owned();
        characters[0]["image"] = Value::String("assets/previous.png".to_owned());
        characters[0]["additionalAssets"] =
            serde_json::json!([["existing", "assets/existing.bin", "bin"]]);
        let mut root = database.clone();
        let root_object = root.as_object_mut().expect("fixture root");
        root_object.remove("characters");
        root_object.remove("botPresets");
        let staging = store.replace_begin().expect("begin replacement");
        store
            .replace_put_root(&staging.staging_id, &root)
            .expect("stage root");
        store
            .replace_put_presets(
                &staging.staging_id,
                database["botPresets"].as_array().expect("fixture presets"),
            )
            .expect("stage presets");
        store
            .replace_add_characters(&staging.staging_id, &characters)
            .expect("stage characters");
        store
            .replace_commit(&staging.staging_id, Some(0))
            .expect("activate fixture");

        let manifest = encode_owner_manifest(&[OwnerManifestEntry {
            tuple: [
                "existing".to_owned(),
                "assets/existing.bin".to_owned(),
                "bin".to_owned(),
            ],
            payload_hash: None,
        }])
        .expect("encode owner manifest");
        let prepared_manifest = PayloadCas::new(directory.path())
            .expect("open fixture CAS")
            .prepare_bytes(&manifest)
            .expect("prepare owner manifest");
        let owner_head = AssetOwnerHead::present(
            AssetOwnerLocator::CharacterAdditionalAssets {
                character_id: character_id.clone(),
            },
            prepared_manifest.content_hash,
            1,
        );
        let character = store
            .read_character(&character_id, None)
            .expect("read character")
            .expect("fixture character")
            .value;
        store
            .commit(&WorkingSetCommit {
                expected_revision: 1,
                root_mutations: None,
                root: None,
                replace_presets: None,
                character: Some(character),
                character_details: None,
                replace_character: None,
                add_character: None,
                conversations: None,
                delete_character_id: None,
                plugin_storage: None,
                asset_owner_heads: Some(vec![owner_head.clone()]),
            })
            .expect("seed owner head");
        (directory, store, character_id, owner_head)
    }

    fn opened_source(path: &std::path::Path, total_bytes: u64) -> OpenedJobSource {
        OpenedJobSource {
            file: std::fs::File::open(path).expect("open JPEG source"),
            total_bytes,
        }
    }

    fn job() -> std::sync::Arc<JobControl> {
        super::super::JobRegistry::default()
            .create(JobKind::ImportJpegAsset)
            .expect("create JPEG job")
    }

    fn appended_charx_jpeg() -> Vec<u8> {
        let mut archive = ZipWriter::new(Cursor::new(Vec::new()));
        archive
            .start_file(
                "card.json",
                FileOptions::default().compression_method(CompressionMethod::Stored),
            )
            .expect("start CharX card entry");
        archive
            .write_all(br#"{"spec":"chara_card_v3","data":{"name":"Appended","assets":[]}}"#)
            .expect("write CharX card entry");
        let mut bytes = vec![0xff, 0xd8, 0xff, 0xe0, 0, 0xff, 0xd9];
        bytes.extend_from_slice(&archive.finish().expect("finish CharX archive").into_inner());
        bytes
    }

    #[test]
    fn imports_exact_jpeg_bytes_and_preserves_extension_and_owner_head() {
        for extension in ["jpg", "jpeg"] {
            let (directory, store, character_id, owner_head) = open_fixture();
            let bytes = [0xff, 0xd8, 0xff, 0xe0, 1, 2, 3, 0xff, 0xd9];
            let source_path = directory.path().join(format!("portrait.{extension}"));
            fs::write(&source_path, bytes).expect("write JPEG source");
            let job = job();
            let result = import_jpeg_asset(
                opened_source(&source_path, bytes.len() as u64),
                &format!("portrait.{extension}"),
                JpegAssetDestination::CurrentCharacterImage {
                    character_id: character_id.clone(),
                },
                2,
                directory.path(),
                store,
                &job,
            )
            .expect("import JPEG asset");

            let reopened = PersistentStore::open(directory.path()).expect("reopen store");
            let logical_id = format!("assets/{}.{}", result.source_sha256, extension);
            let alias = reopened
                .read_asset_alias("asset", &logical_id, None)
                .expect("read alias")
                .expect("JPEG alias");
            assert_eq!(alias.value.ext, extension);
            assert_eq!(alias.value.mime, "image/jpeg");
            assert_eq!(
                PayloadCas::new(directory.path())
                    .expect("open CAS")
                    .read_object(&result.source_sha256)
                    .expect("read CAS object")
                    .expect("JPEG CAS object"),
                bytes
            );
            let character = reopened
                .read_character(&character_id, None)
                .expect("read character")
                .expect("character");
            assert_eq!(character.value["image"], logical_id);
            assert_eq!(
                character.value["ccAssets"]
                    .as_array()
                    .expect("ccAssets")
                    .last(),
                Some(&serde_json::json!({
                    "type": "icon",
                    "name": "iconx",
                    "uri": "assets/previous.png",
                    "ext": "png",
                }))
            );
            assert_eq!(
                reopened
                    .read_asset_owner_head(&owner_head.owner, None)
                    .expect("read owner head")
                    .expect("preserved owner head")
                    .value,
                owner_head
            );
        }
    }

    #[test]
    fn replacing_a_native_jpeg_preserves_its_extension_in_previous_portraits() {
        let (directory, store, character_id, _owner_head) = open_fixture();
        let first_bytes = [0xff, 0xd8, 0xff, 0xe0, 1, 2, 3, 0xff, 0xd9];
        let first_path = directory.path().join("first.JPEG");
        fs::write(&first_path, first_bytes).expect("write first JPEG source");
        let first = import_jpeg_asset(
            opened_source(&first_path, first_bytes.len() as u64),
            "first.JPEG",
            JpegAssetDestination::CurrentCharacterImage {
                character_id: character_id.clone(),
            },
            2,
            directory.path(),
            store,
            &job(),
        )
        .expect("import first JPEG asset");
        let first_logical_id = format!("assets/{}.jpeg", first.source_sha256);

        let second_bytes = [0xff, 0xd8, 0xff, 0xe1, 4, 5, 6, 0xff, 0xd9];
        let second_path = directory.path().join("second.jpg");
        fs::write(&second_path, second_bytes).expect("write second JPEG source");
        import_jpeg_asset(
            opened_source(&second_path, second_bytes.len() as u64),
            "second.jpg",
            JpegAssetDestination::CurrentCharacterImage {
                character_id: character_id.clone(),
            },
            3,
            directory.path(),
            PersistentStore::open(directory.path()).expect("reopen store for second JPEG"),
            &job(),
        )
        .expect("replace first JPEG asset");

        let reopened = PersistentStore::open(directory.path()).expect("reopen replaced character");
        let character = reopened
            .read_character(&character_id, None)
            .expect("read character")
            .expect("character");
        assert_eq!(
            character.value["ccAssets"]
                .as_array()
                .expect("previous portraits")
                .last(),
            Some(&json!({
                "type": "icon",
                "name": "iconx",
                "uri": first_logical_id,
                "ext": "jpeg",
            }))
        );
    }

    #[test]
    fn previous_portrait_extension_ignores_query_and_fragment() {
        let updated = update_character_image(
            json!({
                "chaId": "character-1",
                "image": "assets/portrait.JpG?download=1#preview",
            }),
            "character-1",
            "assets/replacement.png",
        )
        .expect("update character image");

        assert_eq!(updated["ccAssets"][0]["ext"], "jpg");
    }

    #[test]
    fn conflict_cancel_and_source_failure_preserve_the_previous_image_and_owner_head() {
        for failure in ["conflict", "cancel", "source-length"] {
            let (directory, store, character_id, owner_head) = open_fixture();
            let bytes = [0xff, 0xd8, 0xff, 0xe0, 1, 2, 3, 0xff, 0xd9];
            let source_path = directory.path().join("portrait.jpeg");
            fs::write(&source_path, bytes).expect("write JPEG source");
            let job = job();
            let expected_revision = if failure == "conflict" { 1 } else { 2 };
            let total_bytes = if failure == "source-length" {
                bytes.len() as u64 + 1
            } else {
                bytes.len() as u64
            };
            if failure == "cancel" {
                job.request_cancel().expect("request cancellation");
            }
            let error = import_jpeg_asset(
                opened_source(&source_path, total_bytes),
                "portrait.jpeg",
                JpegAssetDestination::CurrentCharacterImage {
                    character_id: character_id.clone(),
                },
                expected_revision,
                directory.path(),
                store,
                &job,
            )
            .expect_err("reject failed JPEG import");
            assert_eq!(
                error.code,
                match failure {
                    "conflict" => "revision-conflict",
                    "cancel" => "cancelled",
                    "source-length" => "invalid-input",
                    _ => unreachable!(),
                }
            );

            let reopened = PersistentStore::open(directory.path()).expect("reopen store");
            assert_eq!(reopened.revision().expect("read revision"), 2);
            assert_eq!(
                reopened
                    .read_character(&character_id, None)
                    .expect("read character")
                    .expect("character")
                    .value["image"],
                "assets/previous.png"
            );
            assert_eq!(
                reopened
                    .read_asset_owner_head(&owner_head.owner, None)
                    .expect("read owner head")
                    .expect("owner head")
                    .value,
                owner_head
            );
        }
    }

    #[test]
    fn rejects_a_missing_destination_owner_without_mutating_the_library() {
        let (directory, store, character_id, owner_head) = open_fixture();
        let bytes = [0xff, 0xd8, 0xff, 0xe0, 1, 2, 3, 0xff, 0xd9];
        let source_path = directory.path().join("portrait.jpeg");
        fs::write(&source_path, bytes).expect("write JPEG source");

        let error = import_jpeg_asset(
            opened_source(&source_path, bytes.len() as u64),
            "portrait.jpeg",
            JpegAssetDestination::CurrentCharacterImage {
                character_id: "missing-character".to_owned(),
            },
            2,
            directory.path(),
            store,
            &job(),
        )
        .expect_err("reject missing destination");
        assert_eq!(error.code, "invalid-destination");

        let reopened = PersistentStore::open(directory.path()).expect("reopen store");
        assert_eq!(reopened.revision().expect("read revision"), 2);
        assert_eq!(
            reopened
                .read_character(&character_id, None)
                .expect("read character")
                .expect("character")
                .value["image"],
            "assets/previous.png"
        );
        assert_eq!(
            reopened
                .read_asset_owner_head(&owner_head.owner, None)
                .expect("read owner head")
                .expect("owner head")
                .value,
            owner_head
        );
    }

    #[test]
    fn rejects_appended_charx_jpeg_without_mutating_the_destination() {
        let (directory, store, character_id, owner_head) = open_fixture();
        let bytes = appended_charx_jpeg();
        let source_path = directory.path().join("appended.jpeg");
        fs::write(&source_path, &bytes).expect("write appended CharX JPEG");

        let error = import_jpeg_asset(
            opened_source(&source_path, bytes.len() as u64),
            "appended.jpeg",
            JpegAssetDestination::CurrentCharacterImage {
                character_id: character_id.clone(),
            },
            2,
            directory.path(),
            store,
            &job(),
        )
        .expect_err("reject appended CharX JPEG");
        assert_eq!(error.code, "invalid-input");

        let reopened = PersistentStore::open(directory.path()).expect("reopen store");
        assert_eq!(reopened.revision().expect("read revision"), 2);
        assert_eq!(
            reopened
                .read_character(&character_id, None)
                .expect("read character")
                .expect("character")
                .value["image"],
            "assets/previous.png"
        );
        assert_eq!(
            reopened
                .read_asset_owner_head(&owner_head.owner, None)
                .expect("read owner head")
                .expect("owner head")
                .value,
            owner_head
        );
    }

    #[test]
    fn final_revision_race_preserves_destination_and_releases_durable_cas_root() {
        let (directory, store, character_id, owner_head) = open_fixture();
        let bytes = [0xff, 0xd8, 0xff, 0xe0, 1, 2, 3, 0xff, 0xd9];
        let object_hash = hex::encode(Sha256::digest(bytes));
        let source_path = directory.path().join("portrait.jpeg");
        fs::write(&source_path, bytes).expect("write JPEG source");
        let job = job();
        let job_id = job.id();

        let error = import_jpeg_asset_with_before_commit(
            opened_source(&source_path, bytes.len() as u64),
            "portrait.jpeg",
            JpegAssetDestination::CurrentCharacterImage {
                character_id: character_id.clone(),
            },
            2,
            directory.path(),
            store,
            &job,
            || {
                let mut racing_store =
                    PersistentStore::open(directory.path()).expect("open racing store");
                let mut root = racing_store
                    .read_root(None)
                    .expect("read racing root")
                    .value;
                root["username"] = Value::String("revision-race".to_owned());
                racing_store
                    .commit(&WorkingSetCommit {
                        expected_revision: 2,
                        root_mutations: None,
                        root: Some(root),
                        replace_presets: None,
                        character: None,
                        character_details: None,
                        replace_character: None,
                        add_character: None,
                        conversations: None,
                        delete_character_id: None,
                        plugin_storage: None,
                        asset_owner_heads: None,
                    })
                    .expect("win final revision race");
                Ok(())
            },
        )
        .expect_err("reject stale JPEG activation");

        assert_eq!(error.code, "revision-conflict");
        let reopened = PersistentStore::open(directory.path()).expect("reopen store");
        assert_eq!(reopened.revision().expect("read revision"), 3);
        assert_eq!(
            reopened
                .read_character(&character_id, None)
                .expect("read character")
                .expect("character")
                .value["image"],
            "assets/previous.png"
        );
        assert_eq!(
            reopened
                .read_asset_owner_head(&owner_head.owner, None)
                .expect("read owner head")
                .expect("owner head")
                .value,
            owner_head
        );
        assert!(!directory
            .path()
            .join("assets-v2")
            .join("job-pins")
            .join(format!("job-{job_id}.journal"))
            .exists());
        let durable_roots = collect_durable_cas_job_roots(directory.path());
        assert!(!durable_roots.object_hashes.contains(&object_hash));
        let gc = reopened
            .asset_gc_dry_run(16, None, i64::MAX, 0)
            .expect("classify abandoned JPEG object")
            .report;
        assert!(gc.potential_delete_hashes.contains(&object_hash));
    }
}
