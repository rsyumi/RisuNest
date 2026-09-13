use crate::server_sync::residency::RemotePayloadAccess;
#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset_repository::{owner_manifest_codec, PayloadCas};
    use crate::import_export_jobs::{parse_json_card, JobStaging};
    use crate::native_file_jobs::content::content_classification_limits;
    use crate::native_file_jobs::{JobKind, JobRegistry};
    use crate::persistent_store::{
        AssetAlias, AssetOwnerHead, AssetOwnerLocator, AssetRepositoryAuthorityState,
        PersistentStore, StoreError,
    };
    use serde_json::{json, Value};
    use sha2::{Digest, Sha256};
    use std::cell::Cell;
    use std::fs;
    use tempfile::TempDir;

    const IMPORTER_JSON_LIMIT: usize = JSON_CARD_MAX_METADATA_BYTES;

    struct Fixture {
        directory: TempDir,
        store: PersistentStore,
        revision: i64,
        metadata: Value,
        payload_hashes: Vec<String>,
    }

    fn alias(
        key: &str,
        payload: &[u8],
        extension: &str,
        mime: &str,
        cas: &PayloadCas,
    ) -> AssetAlias {
        let prepared = cas.prepare_bytes(payload).unwrap();
        AssetAlias {
            key: key.to_owned(),
            object_hash: Some(prepared.content_hash),
            kind: "asset".to_owned(),
            size: prepared.byte_size as i64,
            mime: mime.to_owned(),
            name: key.to_owned(),
            ext: extension.to_owned(),
            inlay_type: None,
            width: None,
            height: None,
            metadata: json!({}),
        }
    }

    fn fixture() -> Fixture {
        fixture_with_cc_mime("application/octet-stream")
    }

    fn fixture_with_cc_mime(cc_mime: &str) -> Fixture {
        let directory = TempDir::new().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let portrait = alias(
            "assets/portrait.jpg",
            b"portrait-payload",
            "jpg",
            "image/jpeg",
            &cas,
        );
        let cc = alias("assets/cc.dat", b"cc-payload", "DAT", cc_mime, &cas);
        let shared = alias(
            "assets/shared.bin",
            b"first-occurrence",
            "BIN",
            "application/octet-stream",
            &cas,
        );
        let second = cas.prepare_bytes(b"second-occurrence").unwrap();
        let manifest_bytes = owner_manifest_codec::encode_owner_manifest(&[
            owner_manifest_codec::OwnerManifestEntry {
                tuple: ["first".to_owned(), shared.key.clone(), "BIN".to_owned()],
                payload_hash: Some(
                    hex::decode(shared.object_hash.as_ref().unwrap())
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
            },
            owner_manifest_codec::OwnerManifestEntry {
                tuple: ["second".to_owned(), shared.key.clone(), "bIn".to_owned()],
                payload_hash: Some(
                    hex::decode(&second.content_hash)
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
            },
        ])
        .unwrap();
        let manifest = cas.prepare_bytes(&manifest_bytes).unwrap();
        let character = json!({
            "type": "character",
            "chaId": "json-character",
            "name": "JSON Character",
            "image": portrait.key.clone(),
            "ccAssets": [{
                "type": "x-custom",
                "uri": cc.key.clone(),
                "name": "custom",
                "ext": "DAT"
            }],
            "additionalAssets": [
                ["first", shared.key.clone(), "BIN"],
                ["second", shared.key.clone(), "bIn"]
            ],
            "emotionImages": [],
            "triggerscript": [{"comment": "trigger"}],
            "customscript": [{"comment": "regex"}],
            "chats": []
        });
        let staging = store.replace_begin().unwrap().staging_id;
        store.replace_put_root(&staging, &json!({})).unwrap();
        store.replace_put_presets(&staging, &[]).unwrap();
        store
            .replace_add_characters(&staging, &[character])
            .unwrap();
        store
            .replace_put_asset_aliases(&staging, &[portrait.clone(), cc.clone(), shared.clone()])
            .unwrap();
        store
            .replace_put_asset_owner_heads(
                &staging,
                &[AssetOwnerHead::present(
                    AssetOwnerLocator::CharacterAdditionalAssets {
                        character_id: "json-character".to_owned(),
                    },
                    manifest.content_hash,
                    2,
                )],
            )
            .unwrap();
        store
            .replace_put_asset_repository_authority(
                &staging,
                &AssetRepositoryAuthorityState::V2 {
                    migration_id: "character-json-test".to_owned(),
                    compatibility_hash: "ab".repeat(32),
                },
            )
            .unwrap();
        let revision = store.replace_commit(&staging, Some(0)).unwrap().revision;
        let metadata = json!({
            "spec": "chara_card_v3",
            "spec_version": "3.0",
            "data": {
                "name": "JSON Character",
                "extensions": {"risuai": {
                    "triggerscript": [{"comment": "trigger"}],
                    "customScripts": [{"comment": "regex"}]
                }},
                "assets": [
                    {"type": "x-custom", "uri": cc.key, "name": "custom", "ext": "DAT"},
                    {"type": "x-risu-asset", "uri": shared.key, "name": "first", "ext": "BIN"},
                    {"type": "x-risu-asset", "uri": shared.key, "name": "second", "ext": "bIn"},
                    {"type": "icon", "uri": "ccdefault:", "name": "main", "ext": "png"}
                ]
            }
        });
        Fixture {
            directory,
            store,
            revision,
            metadata,
            payload_hashes: vec![
                cc.object_hash.unwrap(),
                shared.object_hash.unwrap(),
                second.content_hash,
                portrait.object_hash.unwrap(),
            ],
        }
    }

    #[test]
    fn native_json_export_streams_compact_exact_leased_payloads_and_roundtrips_the_importer() {
        let mut fixture = fixture();
        let prepared = fixture
            .store
            .prepare_risu_save_export(fixture.revision)
            .unwrap();
        let owned = fixture.directory.path().join("owned");
        let chosen = fixture.directory.path().join("chosen");
        fs::create_dir(&owned).unwrap();
        fs::create_dir(&chosen).unwrap();
        let destination = chosen.join("character.json");
        let job = JobRegistry::default()
            .create(JobKind::ExportCharacterCard)
            .unwrap();

        let result = export_character_json(
            prepared,
            "json-character",
            fixture.metadata,
            &owned,
            &fixture.directory.path().join("handoffs"),
            Some(&destination),
            &job,
        )
        .unwrap();

        assert_eq!(result.revision, fixture.revision);
        let bytes = fs::read(&destination).unwrap();
        assert!(bytes.len() <= IMPORTER_JSON_LIMIT);
        assert!(!bytes.contains(&b'\n'));
        assert!(String::from_utf8_lossy(&bytes).contains("data:image/jpeg;base64,"));
        let parse_root = fixture.directory.path().join("json-parse");
        fs::create_dir(&parse_root).unwrap();
        let staging = JobStaging::open(&parse_root).unwrap();
        let parsed = parse_json_card(
            &mut bytes.as_slice(),
            &staging,
            &content_classification_limits(),
            &|| false,
        )
        .unwrap();
        assert_eq!(
            parsed
                .payloads
                .iter()
                .map(|payload| payload.payload.sha256.clone())
                .collect::<Vec<_>>(),
            fixture.payload_hashes
        );
        assert_eq!(
            parsed
                .metadata
                .pointer("/data/extensions/risuai/triggerscript"),
            Some(&json!([{"comment": "trigger"}]))
        );
        assert_eq!(
            parsed
                .metadata
                .pointer("/data/extensions/risuai/customScripts"),
            Some(&json!([{"comment": "regex"}]))
        );
    }

    #[test]
    fn native_json_export_cancellation_and_importer_limit_preserve_the_destination() {
        for cancel in [true, false] {
            let mut fixture = fixture();
            if !cancel {
                fixture.metadata["data"]["description"] =
                    Value::String("x".repeat(IMPORTER_JSON_LIMIT));
            }
            let prepared = fixture
                .store
                .prepare_risu_save_export(fixture.revision)
                .unwrap();
            let owned = fixture.directory.path().join("owned");
            let chosen = fixture.directory.path().join("chosen");
            fs::create_dir(&owned).unwrap();
            fs::create_dir(&chosen).unwrap();
            let destination = chosen.join("character.json");
            fs::write(&destination, b"previous JSON").unwrap();
            let job = JobRegistry::default()
                .create(JobKind::ExportCharacterCard)
                .unwrap();
            if cancel {
                job.request_cancel().unwrap();
            }

            let error = export_character_json(
                prepared,
                "json-character",
                fixture.metadata,
                &owned,
                &fixture.directory.path().join("handoffs"),
                Some(&destination),
                &job,
            )
            .unwrap_err();

            assert_eq!(
                error.code,
                if cancel { "cancelled" } else { "invalid-input" }
            );
            assert_eq!(fs::read(destination).unwrap(), b"previous JSON");
        }
    }

    #[test]
    fn native_json_export_rejects_a_stale_revision_before_publication() {
        let mut fixture = fixture();
        let error = fixture
            .store
            .prepare_risu_save_export(fixture.revision + 1)
            .err()
            .unwrap();
        assert!(matches!(error, StoreError::RevisionConflict { .. }));
    }

    #[test]
    fn portable_json_export_returns_the_exact_managed_handoff_name() {
        let mut fixture = fixture();
        let prepared = fixture
            .store
            .prepare_risu_save_export(fixture.revision)
            .unwrap();
        let owned = fixture.directory.path().join("owned");
        let handoffs = fixture.directory.path().join("handoffs");
        fs::create_dir(&owned).unwrap();
        let job = JobRegistry::default()
            .create(JobKind::ExportCharacterCard)
            .unwrap();

        let result = export_character_json(
            prepared,
            "json-character",
            fixture.metadata,
            &owned,
            &handoffs,
            None,
            &job,
        )
        .unwrap();

        let handoff = result.handoff_path.map(std::path::PathBuf::from).unwrap();
        assert_eq!(handoff.parent(), Some(handoffs.as_path()));
        let name = handoff.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("risu-character-card-"));
        assert!(name.ends_with(".json"));
        assert_eq!(
            result.source_sha256,
            hex::encode(Sha256::digest(fs::read(handoff).unwrap()))
        );
    }

    #[test]
    fn release_failure_prevents_desktop_json_publication_and_releases_exactly_once() {
        let mut fixture = fixture();
        let prepared = fixture
            .store
            .prepare_risu_save_export(fixture.revision)
            .unwrap();
        let owned = fixture.directory.path().join("owned");
        let chosen = fixture.directory.path().join("chosen");
        fs::create_dir(&owned).unwrap();
        fs::create_dir(&chosen).unwrap();
        let destination = chosen.join("character.json");
        fs::write(&destination, b"previous JSON").unwrap();
        let job = JobRegistry::default()
            .create(JobKind::ExportCharacterCard)
            .unwrap();
        let release_calls = Cell::new(0);

        let error = export_character_json_with_release(
            prepared,
            "json-character",
            fixture.metadata,
            &owned,
            &fixture.directory.path().join("handoffs"),
            Some(&destination),
            &job,
            JSON_CARD_MAX_METADATA_BYTES,
            |prepared: &mut PreparedRisuSaveExport| {
                release_calls.set(release_calls.get() + 1);
                prepared.release_reader()?;
                Err(StoreError::Store {
                    message: "injected checkpoint failure".to_owned(),
                })
            },
        )
        .unwrap_err();

        assert_eq!(error.code, "store-error");
        assert_eq!(release_calls.get(), 1);
        assert_eq!(fs::read(destination).unwrap(), b"previous JSON");
    }

    #[test]
    fn release_failure_does_not_create_a_portable_json_handoff() {
        let mut fixture = fixture();
        let prepared = fixture
            .store
            .prepare_risu_save_export(fixture.revision)
            .unwrap();
        let owned = fixture.directory.path().join("owned");
        let handoffs = fixture.directory.path().join("handoffs");
        fs::create_dir(&owned).unwrap();
        let job = JobRegistry::default()
            .create(JobKind::ExportCharacterCard)
            .unwrap();
        let release_calls = Cell::new(0);

        let error = export_character_json_with_release(
            prepared,
            "json-character",
            fixture.metadata,
            &owned,
            &handoffs,
            None,
            &job,
            JSON_CARD_MAX_METADATA_BYTES,
            |prepared: &mut PreparedRisuSaveExport| {
                release_calls.set(release_calls.get() + 1);
                prepared.release_reader()?;
                Err(StoreError::Store {
                    message: "injected checkpoint failure".to_owned(),
                })
            },
        )
        .unwrap_err();

        assert_eq!(error.code, "store-error");
        assert_eq!(release_calls.get(), 1);
        assert!(!handoffs.exists() || fs::read_dir(handoffs).unwrap().next().is_none());
    }

    #[test]
    fn native_json_export_still_rejects_malformed_nonempty_mime() {
        let mut fixture = fixture_with_cc_mime("not a media type");
        let prepared = fixture
            .store
            .prepare_risu_save_export(fixture.revision)
            .unwrap();
        let owned = fixture.directory.path().join("owned");
        fs::create_dir(&owned).unwrap();
        let job = JobRegistry::default()
            .create(JobKind::ExportCharacterCard)
            .unwrap();

        let error = export_character_json(
            prepared,
            "json-character",
            fixture.metadata,
            &owned,
            &fixture.directory.path().join("handoffs"),
            None,
            &job,
        )
        .unwrap_err();

        assert_eq!(error.code, "invalid-input");
        assert!(error.message.contains("MIME"));
    }
}
use super::content::JSON_CARD_MAX_METADATA_BYTES;
use super::error::{
    cancelled, destination_error_with, invalid_input, io_error, job_error, store_error,
};
use super::{JobControl, JobPhase, JobProgress, JobResultSummary, NativeJobError};
use crate::asset_repository::{owner_manifest_codec::OwnerManifestEntry, PayloadCas};
use crate::persistent_store::export::{self, destination};
use crate::persistent_store::{PreparedRisuSaveExport, RevisionReadLease, StoreResult};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, write::EncoderWriter};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::fs::{self, OpenOptions};
use std::io::{self, BufWriter, Cursor, Read, Write};
use std::path::Path;
use uuid::Uuid;

const COPY_BUFFER_BYTES: usize = 64 * 1024;
const JSON_LIMIT_MESSAGE: &str = "native JSON character card exceeds the 128 MiB importer limit";
pub(super) const FALLBACK_PORTRAIT: &[u8] = include_bytes!("../../../public/none.webp");

pub(super) enum JsonAssetSource {
    Cas {
        key: String,
        hash: String,
        size: u64,
        mime: String,
    },
    FallbackPortrait,
}

pub(crate) fn export_character_json(
    prepared: PreparedRisuSaveExport,
    character_id: &str,
    metadata: Value,
    owned_directory: &Path,
    handoff_directory: &Path,
    destination_path: Option<&Path>,
    job: &JobControl,
) -> Result<JobResultSummary, NativeJobError> {
    export_character_json_with_release(
        prepared,
        character_id,
        metadata,
        owned_directory,
        handoff_directory,
        destination_path,
        job,
        JSON_CARD_MAX_METADATA_BYTES,
        PreparedRisuSaveExport::release_reader,
    )
}

#[allow(clippy::too_many_arguments)]
fn export_character_json_with_release<F>(
    mut prepared: PreparedRisuSaveExport,
    character_id: &str,
    metadata: Value,
    owned_directory: &Path,
    handoff_directory: &Path,
    destination_path: Option<&Path>,
    job: &JobControl,
    maximum_bytes: usize,
    release_reader: F,
) -> Result<JobResultSummary, NativeJobError>
where
    F: FnOnce(&mut PreparedRisuSaveExport) -> StoreResult<()>,
{
    let mut release_reader = Some(release_reader);
    let outcome = export_character_json_with_reader(
        &mut prepared,
        character_id,
        metadata,
        owned_directory,
        handoff_directory,
        destination_path,
        job,
        maximum_bytes,
        &mut release_reader,
    );
    match release_reader.take() {
        Some(release_reader) => super::error::finish_with_release(
            outcome,
            release_reader(&mut prepared),
            "revision release failed",
        ),
        None => outcome,
    }
}

#[allow(clippy::too_many_arguments)]
fn export_character_json_with_reader<F>(
    prepared: &mut PreparedRisuSaveExport,
    character_id: &str,
    metadata: Value,
    owned_directory: &Path,
    handoff_directory: &Path,
    destination_path: Option<&Path>,
    job: &JobControl,
    maximum_bytes: usize,
    release_reader: &mut Option<F>,
) -> Result<JobResultSummary, NativeJobError>
where
    F: FnOnce(&mut PreparedRisuSaveExport) -> StoreResult<()>,
{
    if job.is_cancel_requested() {
        return Err(cancelled("character JSON export cancelled before encoding"));
    }
    job.start(JobPhase::WritingExport).map_err(job_error)?;
    let repository =
        PayloadCas::new(prepared.repository_root().map_err(store_error)?).map_err(io_error)?;
    let sources = {
        let reader = prepared.reader().map_err(store_error)?;
        let projected = export::projected_character(
            &reader.connection,
            &prepared.snapshots_dir,
            &reader.target,
            character_id,
        )
        .map_err(store_error)?;
        validate_metadata(&projected.value, character_id, &metadata)?;
        json_asset_sources(
            &projected.value,
            projected.additional_asset_entries.as_deref(),
            &metadata,
            reader,
            &repository,
        )?
    };
    let total_items = u64::try_from(sources.iter().filter(|source| source.is_some()).count())
        .map_err(|_| invalid_input("character JSON asset count is invalid"))?;
    job.set_progress(JobProgress {
        completed_bytes: 0,
        total_bytes: None,
        completed_items: 0,
        total_items: Some(total_items),
    })
    .map_err(job_error)?;

    let source = owned_directory.join("character.json");
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&source)
        .map_err(io_error)?;
    let output = BufWriter::with_capacity(COPY_BUFFER_BYTES, file);
    let mut output = LimitedWriter::new(output, maximum_bytes);
    write_json_value(
        &mut output,
        &metadata,
        JsonLocation::Root,
        &sources,
        &repository,
        job,
    )?;
    output.flush().map_err(json_io_error)?;
    output.inner.get_ref().sync_all().map_err(io_error)?;
    drop(output);
    let source_bytes = fs::metadata(&source).map_err(io_error)?.len();
    job.set_progress(JobProgress {
        completed_bytes: source_bytes,
        total_bytes: Some(source_bytes.saturating_mul(2)),
        completed_items: total_items,
        total_items: Some(total_items),
    })
    .map_err(job_error)?;

    if job.is_cancel_requested() {
        return Err(cancelled(
            "character JSON export cancelled before destination publication",
        ));
    }
    release_reader
        .take()
        .ok_or_else(|| NativeJobError::new("store-error", "revision release was already used"))?(
        prepared,
    )
    .map_err(store_error)?;
    job.set_phase(JobPhase::PublishingDestination)
        .map_err(job_error)?;
    let (destination_root, destination_path, handoff_path) = match destination_path {
        Some(destination_path) => (
            destination_path.parent().ok_or_else(|| {
                NativeJobError::new(
                    "invalid-destination",
                    "character JSON destination directory is unavailable",
                )
            })?,
            destination_path.to_owned(),
            None,
        ),
        None => {
            fs::create_dir_all(handoff_directory).map_err(io_error)?;
            let path =
                handoff_directory.join(format!("risu-character-card-{}.json", Uuid::new_v4()));
            (
                handoff_directory,
                path.clone(),
                Some(path.to_string_lossy().into_owned()),
            )
        }
    };
    let phase_failure = RefCell::new(None);
    let published = destination::write_character_card_destination_controlled(
        owned_directory,
        &source,
        destination_root,
        &destination_path,
        || job.is_cancel_requested() || phase_failure.borrow().is_some(),
        |progress| {
            if let Err(error) = job.set_progress(JobProgress {
                completed_bytes: source_bytes.saturating_add(progress.copied_bytes),
                total_bytes: Some(source_bytes.saturating_mul(2)),
                completed_items: total_items,
                total_items: Some(total_items),
            }) {
                *phase_failure.borrow_mut() = Some(error);
            }
        },
        || {
            if job.is_cancel_requested() || phase_failure.borrow().is_some() {
                return Err(destination::DestinationWriteError::Cancelled);
            }
            job.set_phase(JobPhase::FinalizingExport).map_err(|error| {
                *phase_failure.borrow_mut() = Some(error);
                destination::DestinationWriteError::Cancelled
            })
        },
    )
    .map_err(|error| {
        phase_failure
            .into_inner()
            .map(job_error)
            .unwrap_or_else(|| destination_error(error))
    })?;

    Ok(JobResultSummary {
        revision: prepared.revision,
        source_bytes: published.bytes,
        source_sha256: published.sha256,
        character_count: 1,
        preset_count: 0,
        warning_codes: Vec::new(),
        handoff_path,
        publication: None,
    })
}

pub(super) fn validate_metadata(
    character: &Value,
    character_id: &str,
    metadata: &Value,
) -> Result<(), NativeJobError> {
    let character = character
        .as_object()
        .ok_or_else(|| invalid_input("pinned character must be an object"))?;
    if character.get("chaId").and_then(Value::as_str) != Some(character_id) {
        return Err(invalid_input("pinned character identity changed"));
    }
    if metadata.get("spec").and_then(Value::as_str) != Some("chara_card_v3")
        || metadata.get("spec_version").and_then(Value::as_str) != Some("3.0")
    {
        return Err(invalid_input(
            "native character JSON export requires CCv3 metadata",
        ));
    }
    let data = metadata
        .get("data")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_input("CCv3 card data must be an object"))?;
    if data.get("name") != character.get("name") {
        return Err(invalid_input(
            "CCv3 metadata does not match the pinned character",
        ));
    }
    if data.get("assets") != Some(&Value::Array(expected_card_assets(character)?)) {
        return Err(invalid_input(
            "CCv3 assets do not match the pinned character projection",
        ));
    }
    let risuai = metadata
        .pointer("/data/extensions/risuai")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_input("CCv3 risuai extensions must be an object"))?;
    for (card_key, character_key) in [
        ("triggerscript", "triggerscript"),
        ("customScripts", "customscript"),
    ] {
        let expected = character
            .get(character_key)
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));
        if risuai.get(card_key) != Some(&expected) {
            return Err(invalid_input(
                "CCv3 script metadata does not match the pinned character",
            ));
        }
    }
    Ok(())
}

fn expected_card_assets(character: &Map<String, Value>) -> Result<Vec<Value>, NativeJobError> {
    let mut assets = character
        .get("ccAssets")
        .map(|value| {
            value
                .as_array()
                .cloned()
                .ok_or_else(|| invalid_input("pinned character ccAssets must be an array"))
        })
        .transpose()?
        .unwrap_or_default();
    if let Some(additional) = character.get("additionalAssets") {
        for tuple in additional
            .as_array()
            .ok_or_else(|| invalid_input("pinned character additionalAssets must be an array"))?
        {
            let tuple = tuple.as_array().ok_or_else(|| {
                invalid_input("pinned character additional asset must be a tuple")
            })?;
            assets.push(json!({
                "type": "x-risu-asset",
                "uri": tuple.get(1).and_then(Value::as_str).unwrap_or_default(),
                "name": tuple.first().and_then(Value::as_str).unwrap_or_default(),
                "ext": tuple.get(2).and_then(Value::as_str).filter(|value| !value.is_empty()).unwrap_or("png"),
            }));
        }
    }
    if let Some(emotions) = character.get("emotionImages") {
        for tuple in emotions
            .as_array()
            .ok_or_else(|| invalid_input("pinned character emotionImages must be an array"))?
        {
            let tuple = tuple
                .as_array()
                .ok_or_else(|| invalid_input("pinned character emotion image must be a tuple"))?;
            assets.push(json!({
                "type": "emotion",
                "uri": tuple.get(1).and_then(Value::as_str).unwrap_or_default(),
                "name": tuple.first().and_then(Value::as_str).unwrap_or_default(),
                "ext": "png",
            }));
        }
        assets.push(json!({
            "type": "icon",
            "uri": "ccdefault:",
            "name": "main",
            "ext": "png",
        }));
    }
    Ok(assets)
}

pub(super) fn json_asset_sources(
    character: &Value,
    additional_entries: Option<&[OwnerManifestEntry]>,
    metadata: &Value,
    reader: &RevisionReadLease,
    repository: &PayloadCas,
) -> Result<Vec<Option<JsonAssetSource>>, NativeJobError> {
    let assets = metadata
        .pointer("/data/assets")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_input("CCv3 assets must be an array"))?;
    let cc_asset_count = character
        .get("ccAssets")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let additional_asset_count = character
        .get("additionalAssets")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    if let Some(entries) = additional_entries {
        if entries.len() != additional_asset_count {
            return Err(invalid_input(
                "pinned character owner manifest occurrence count changed",
            ));
        }
    }
    assets
        .iter()
        .enumerate()
        .map(|(index, asset)| {
            let uri = asset
                .get("uri")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_input("CCv3 asset URI must be a string"))?;
            if uri.starts_with("data:") || uri.starts_with("embeded://") || uri.is_empty() {
                return Err(invalid_input(
                    "live character metadata contains an inline or invalid asset URI",
                ));
            }
            if uri.starts_with("http://") || uri.starts_with("https://") {
                return Ok(None);
            }
            if uri == "ccdefault:" {
                return match character
                    .get("image")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                {
                    Some(key) => cas_source(key, None, reader, repository).map(Some),
                    None => Ok(Some(JsonAssetSource::FallbackPortrait)),
                };
            }
            let occurrence = index
                .checked_sub(cc_asset_count)
                .filter(|offset| *offset < additional_asset_count)
                .and_then(|offset| additional_entries.and_then(|entries| entries.get(offset)));
            cas_source(uri, occurrence, reader, repository).map(Some)
        })
        .collect()
}

fn cas_source(
    key: &str,
    occurrence: Option<&OwnerManifestEntry>,
    reader: &RevisionReadLease,
    repository: &PayloadCas,
) -> Result<JsonAssetSource, NativeJobError> {
    let alias =
        export::pinned_asset_alias(&reader.connection, &reader.target, key).map_err(store_error)?;
    let (hash, size) = match occurrence {
        Some(occurrence) => {
            if occurrence.tuple[1] != key {
                return Err(invalid_input(
                    "CCv3 additional asset occurrence does not match its pinned key",
                ));
            }
            let hash = occurrence.payload_hash.map(hex::encode).ok_or_else(|| {
                invalid_input(format!(
                    "pinned character asset has no native payload: {key}"
                ))
            })?;
            let size = repository
                .stat_available_object(&hash)
                .map_err(io_error)?
                .ok_or_else(|| {
                    invalid_input(format!("pinned character asset is missing: {key}"))
                })?;
            (hash, size)
        }
        None => {
            let hash = alias.object_hash.ok_or_else(|| {
                invalid_input(format!(
                    "pinned character asset has no native payload: {key}"
                ))
            })?;
            let size = u64::try_from(alias.size)
                .map_err(|_| invalid_input("pinned character asset size is invalid"))?;
            (hash, size)
        }
    };
    let mime = if alias.mime.is_empty() {
        "application/octet-stream".to_owned()
    } else if valid_media_type(&alias.mime) {
        alias.mime
    } else {
        return Err(invalid_input("pinned character asset MIME type is invalid"));
    };
    Ok(JsonAssetSource::Cas {
        key: key.to_owned(),
        hash,
        size,
        mime,
    })
}

fn valid_media_type(value: &str) -> bool {
    let mut parts = value.split('/');
    matches!((parts.next(), parts.next(), parts.next()), (Some(kind), Some(subtype), None)
        if valid_mime_token(kind) && valid_mime_token(subtype))
}

fn valid_mime_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'.' | b'+' | b'-'
                )
        })
}

#[derive(Clone, Copy)]
enum JsonLocation {
    Root,
    Data,
    Assets,
    Asset(usize),
    AssetUri(usize),
    Other,
}

fn write_json_value<W: Write>(
    writer: &mut LimitedWriter<W>,
    value: &Value,
    location: JsonLocation,
    sources: &[Option<JsonAssetSource>],
    repository: &PayloadCas,
    job: &JobControl,
) -> Result<(), NativeJobError> {
    match value {
        Value::Object(object) => {
            writer.write_all(b"{").map_err(json_io_error)?;
            for (position, (key, value)) in object.iter().enumerate() {
                if position != 0 {
                    writer.write_all(b",").map_err(json_io_error)?;
                }
                serde_json::to_writer(&mut *writer, key).map_err(json_serde_error)?;
                writer.write_all(b":").map_err(json_io_error)?;
                let child = match (location, key.as_str()) {
                    (JsonLocation::Root, "data") => JsonLocation::Data,
                    (JsonLocation::Data, "assets") => JsonLocation::Assets,
                    (JsonLocation::Asset(index), "uri") => JsonLocation::AssetUri(index),
                    _ => JsonLocation::Other,
                };
                write_json_value(writer, value, child, sources, repository, job)?;
            }
            writer.write_all(b"}").map_err(json_io_error)
        }
        Value::Array(values) => {
            writer.write_all(b"[").map_err(json_io_error)?;
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    writer.write_all(b",").map_err(json_io_error)?;
                }
                let child = if matches!(location, JsonLocation::Assets) {
                    JsonLocation::Asset(index)
                } else {
                    JsonLocation::Other
                };
                write_json_value(writer, value, child, sources, repository, job)?;
            }
            writer.write_all(b"]").map_err(json_io_error)
        }
        Value::String(_) => match location {
            JsonLocation::AssetUri(index) => match sources.get(index).and_then(Option::as_ref) {
                Some(source) => write_data_uri(writer, source, repository, job),
                None => serde_json::to_writer(writer, value).map_err(json_serde_error),
            },
            _ => serde_json::to_writer(writer, value).map_err(json_serde_error),
        },
        _ => serde_json::to_writer(writer, value).map_err(json_serde_error),
    }
}

fn write_data_uri<W: Write>(
    writer: &mut LimitedWriter<W>,
    source: &JsonAssetSource,
    repository: &PayloadCas,
    job: &JobControl,
) -> Result<(), NativeJobError> {
    let (mut input, expected_hash, expected_size, mime): (Box<dyn Read>, String, u64, &str) =
        match source {
            JsonAssetSource::Cas {
                key,
                hash,
                size,
                mime,
            } => (
                Box::new(
                    repository
                        .open_available_object(hash)
                        .map_err(io_error)?
                        .ok_or_else(|| {
                            invalid_input(format!("pinned character asset is missing: {key}"))
                        })?,
                ),
                hash.clone(),
                *size,
                mime,
            ),
            JsonAssetSource::FallbackPortrait => (
                Box::new(Cursor::new(FALLBACK_PORTRAIT)),
                hex::encode(Sha256::digest(FALLBACK_PORTRAIT)),
                FALLBACK_PORTRAIT.len() as u64,
                "image/webp",
            ),
        };
    writer
        .write_all(format!("\"data:{mime};base64,").as_bytes())
        .map_err(json_io_error)?;
    let mut encoder = EncoderWriter::new(&mut *writer, &BASE64_STANDARD);
    let mut hasher = Sha256::new();
    let mut copied = 0_u64;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        if job.is_cancel_requested() {
            return Err(cancelled(
                "character JSON export cancelled while writing assets",
            ));
        }
        let read = input.read(&mut buffer).map_err(io_error)?;
        if read == 0 {
            break;
        }
        copied = copied
            .checked_add(read as u64)
            .ok_or_else(|| invalid_input("character JSON asset length overflowed"))?;
        if copied > expected_size {
            return Err(invalid_input("pinned character asset size changed"));
        }
        hasher.update(&buffer[..read]);
        encoder.write_all(&buffer[..read]).map_err(json_io_error)?;
    }
    encoder.finish().map_err(json_io_error)?;
    drop(encoder);
    if copied != expected_size || hex::encode(hasher.finalize()) != expected_hash {
        return Err(invalid_input("pinned character asset payload hash changed"));
    }
    writer.write_all(b"\"").map_err(json_io_error)
}

struct LimitedWriter<W> {
    inner: W,
    written: usize,
    maximum: usize,
}

impl<W> LimitedWriter<W> {
    fn new(inner: W, maximum: usize) -> Self {
        Self {
            inner,
            written: 0,
            maximum,
        }
    }
}

impl<W: Write> Write for LimitedWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.written.saturating_add(buffer.len()) > self.maximum {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                JSON_LIMIT_MESSAGE,
            ));
        }
        let written = self.inner.write(buffer)?;
        self.written = self.written.saturating_add(written);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn destination_error(error: destination::DestinationWriteError) -> NativeJobError {
    destination_error_with(
        error,
        "character JSON source is invalid",
        "character JSON destination is invalid",
        "character JSON export cancelled before destination replacement",
    )
}

fn json_serde_error(error: serde_json::Error) -> NativeJobError {
    if error.to_string().contains(JSON_LIMIT_MESSAGE) {
        invalid_input(JSON_LIMIT_MESSAGE)
    } else {
        NativeJobError::new("store-error", error.to_string())
    }
}

fn json_io_error(error: io::Error) -> NativeJobError {
    if error.to_string().contains(JSON_LIMIT_MESSAGE) {
        invalid_input(JSON_LIMIT_MESSAGE)
    } else {
        io_error(error)
    }
}
