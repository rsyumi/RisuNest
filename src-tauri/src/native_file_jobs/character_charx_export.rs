use super::{JobControl, JobPhase, JobProgress, JobResultSummary, NativeJobError};
use crate::asset_repository::PayloadCas;
use crate::persistent_store::export::{self, destination};
use crate::persistent_store::{PreparedRisuSaveExport, RevisionReadLease, StoreError};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::path::Path;
use zip::write::FileOptions;
use zip::{CompressionMethod, ZipWriter};

const COPY_BUFFER_BYTES: usize = 64 * 1024;
const CARD_METADATA_LIMIT: usize = 8 * 1024 * 1024;
const RPACK_MAP: &[u8; 512] = include_bytes!("../../../src/ts/rpack/rpack_map.bin");

pub(crate) fn export_character_charx(
    mut prepared: PreparedRisuSaveExport,
    character_id: &str,
    card: Value,
    module: Value,
    owned_directory: &Path,
    destination_path: &Path,
    job: &JobControl,
) -> Result<JobResultSummary, NativeJobError> {
    let reader = prepared.take_reader().map_err(store_error)?;
    let outcome = export_character_charx_with_reader(
        &prepared,
        &reader,
        character_id,
        card,
        module,
        owned_directory,
        destination_path,
        job,
    );
    finish_with_lease(outcome, &prepared, reader)
}

fn export_character_charx_with_reader(
    prepared: &PreparedRisuSaveExport,
    reader: &RevisionReadLease,
    character_id: &str,
    mut card: Value,
    module: Value,
    owned_directory: &Path,
    destination_path: &Path,
    job: &JobControl,
) -> Result<JobResultSummary, NativeJobError> {
    if job.is_cancel_requested() {
        return Err(cancelled(
            "character CharX export cancelled before encoding",
        ));
    }
    job.start(JobPhase::WritingExport).map_err(job_error)?;
    let character = export::projected_character(
        &reader.connection,
        &prepared.snapshots_dir,
        &reader.target,
        character_id,
    )
    .map_err(store_error)?;
    validate_character_identity(&character, character_id, &card)?;
    validate_module_overlay(&character, &card, &module)?;
    let asset_count = card_assets_mut(&mut card)?.len();
    let total_items = u64::try_from(asset_count)
        .unwrap_or(u64::MAX)
        .saturating_add(2);
    job.set_progress(JobProgress {
        completed_bytes: 0,
        total_bytes: None,
        completed_items: 0,
        total_items: Some(total_items),
    })
    .map_err(job_error)?;

    let repository =
        PayloadCas::new(prepared.repository_root().map_err(store_error)?).map_err(io_error)?;
    let source = owned_directory.join("character.charx");
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&source)
        .map_err(io_error)?;
    let mut archive = ZipWriter::new(BufWriter::with_capacity(COPY_BUFFER_BYTES, file));
    let mut completed_bytes = 0_u64;
    let mut completed_items = 0_u64;

    let assets = card_assets_mut(&mut card)?;
    for (index, asset) in assets.iter_mut().enumerate() {
        if job.is_cancel_requested() {
            return Err(cancelled(
                "character CharX export cancelled while writing assets",
            ));
        }
        let Some(key) = embedded_asset_key(asset, &character)? else {
            completed_items += 1;
            job.set_progress(JobProgress {
                completed_bytes,
                total_bytes: None,
                completed_items,
                total_items: Some(total_items),
            })
            .map_err(job_error)?;
            continue;
        };
        let alias = export::pinned_asset_alias(&reader.connection, &reader.target, &key)
            .map_err(store_error)?;
        let hash = alias.object_hash.as_deref().ok_or_else(|| {
            invalid_input(format!(
                "pinned character asset has no native payload: {key}"
            ))
        })?;
        let extension = validate_extension(&alias.ext)?;
        let archive_path = archive_asset_path(asset, index, extension)?;
        asset
            .as_object_mut()
            .expect("validated card asset object")
            .insert(
                "uri".to_owned(),
                Value::String(format!("embeded://{archive_path}")),
            );
        asset
            .as_object_mut()
            .expect("validated card asset object")
            .insert("ext".to_owned(), Value::String(extension.to_owned()));

        let source_file = repository
            .open_object(hash)
            .map_err(io_error)?
            .ok_or_else(|| invalid_input(format!("pinned character asset is missing: {key}")))?;
        archive
            .start_file(
                archive_path,
                FileOptions::default()
                    .compression_method(CompressionMethod::Stored)
                    .large_file(alias.size >= i64::from(u32::MAX)),
            )
            .map_err(zip_error)?;
        let copied = copy_verified_object(&mut archive, source_file, hash, alias.size, job)?;
        completed_bytes = completed_bytes.saturating_add(copied);
        completed_items += 1;
        job.set_progress(JobProgress {
            completed_bytes,
            total_bytes: None,
            completed_items,
            total_items: Some(total_items),
        })
        .map_err(job_error)?;
    }

    write_module_overlay(&mut archive, module, job)?;
    completed_items += 1;
    job.set_progress(JobProgress {
        completed_bytes,
        total_bytes: None,
        completed_items,
        total_items: Some(total_items),
    })
    .map_err(job_error)?;
    write_card_metadata(&mut archive, &card, job)?;
    completed_items += 1;
    let mut output = archive.finish().map_err(zip_error)?;
    output.flush().map_err(io_error)?;
    output.get_ref().sync_all().map_err(io_error)?;
    drop(output);
    let source_bytes = fs::metadata(&source).map_err(io_error)?.len();
    job.set_progress(JobProgress {
        completed_bytes: source_bytes,
        total_bytes: Some(source_bytes.saturating_mul(2)),
        completed_items,
        total_items: Some(total_items),
    })
    .map_err(job_error)?;

    if job.is_cancel_requested() {
        return Err(cancelled(
            "character CharX export cancelled before destination publication",
        ));
    }
    job.set_phase(JobPhase::PublishingDestination)
        .map_err(job_error)?;
    let destination_root = destination_path.parent().ok_or_else(|| {
        NativeJobError::new(
            "invalid-destination",
            "character CharX destination directory is unavailable",
        )
    })?;
    let phase_failure = RefCell::new(None);
    let published = destination::write_charx_destination_controlled(
        owned_directory,
        &source,
        destination_root,
        destination_path,
        || job.is_cancel_requested() || phase_failure.borrow().is_some(),
        |progress| {
            if let Err(error) = job.set_progress(JobProgress {
                completed_bytes: source_bytes.saturating_add(progress.copied_bytes),
                total_bytes: Some(source_bytes.saturating_mul(2)),
                completed_items,
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
        handoff_path: None,
        recovery_path: None,
    })
}

fn validate_character_identity(
    character: &Value,
    character_id: &str,
    card: &Value,
) -> Result<(), NativeJobError> {
    let object = character
        .as_object()
        .ok_or_else(|| invalid_input("pinned character must be an object"))?;
    if object.get("chaId").and_then(Value::as_str) != Some(character_id) {
        return Err(invalid_input("pinned character identity changed"));
    }
    if card.get("spec").and_then(Value::as_str) != Some("chara_card_v3")
        || card.get("spec_version").and_then(Value::as_str) != Some("3.0")
    {
        return Err(invalid_input(
            "native character export requires CCv3 metadata",
        ));
    }
    let card_data = card
        .get("data")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_input("CCv3 card data must be an object"))?;
    if card_data.get("name") != object.get("name") {
        return Err(invalid_input(
            "CCv3 metadata does not match the pinned character",
        ));
    }
    let expected_assets = expected_card_assets(object)?;
    if card_data.get("assets") != Some(&Value::Array(expected_assets)) {
        return Err(invalid_input(
            "CCv3 assets do not match the pinned character projection",
        ));
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

fn validate_module_overlay(
    character: &Value,
    card: &Value,
    module: &Value,
) -> Result<(), NativeJobError> {
    let character = character
        .as_object()
        .ok_or_else(|| invalid_input("pinned character must be an object"))?;
    let card_risuai = card
        .pointer("/data/extensions/risuai")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_input("CCv3 risuai extensions must be an object"))?;
    if card_risuai.contains_key("triggerscript") || card_risuai.contains_key("customScripts") {
        return Err(invalid_input(
            "CCv3 module overlay fields must not remain in card metadata",
        ));
    }
    let module = module
        .as_object()
        .ok_or_else(|| invalid_input("character module overlay must be an object"))?;
    for (module_key, character_key) in [
        ("trigger", "triggerscript"),
        ("regex", "customscript"),
        ("lorebook", "globalLore"),
    ] {
        let expected = character
            .get(character_key)
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));
        if module.get(module_key) != Some(&expected) {
            return Err(invalid_input(
                "character module overlay does not match the pinned character",
            ));
        }
    }
    Ok(())
}

fn card_assets_mut(card: &mut Value) -> Result<&mut Vec<Value>, NativeJobError> {
    card.get_mut("data")
        .and_then(Value::as_object_mut)
        .and_then(|data| data.get_mut("assets"))
        .and_then(Value::as_array_mut)
        .ok_or_else(|| invalid_input("CCv3 assets must be an array"))
}

fn embedded_asset_key(asset: &Value, character: &Value) -> Result<Option<String>, NativeJobError> {
    let asset = asset
        .as_object()
        .ok_or_else(|| invalid_input("CCv3 asset must be an object"))?;
    let uri = asset
        .get("uri")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_input("CCv3 asset URI must be a string"))?;
    if uri == "ccdefault:" {
        return character
            .get("image")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .map(Some)
            .ok_or_else(|| invalid_input("pinned character portrait is missing"));
    }
    if uri.starts_with("http://") || uri.starts_with("https://") {
        return Ok(None);
    }
    if uri.starts_with("embeded://") {
        return Err(invalid_input(
            "live character metadata cannot reference an unleased embedded path",
        ));
    }
    if uri.is_empty() {
        return Err(invalid_input("CCv3 asset URI must not be empty"));
    }
    Ok(Some(uri.to_owned()))
}

fn validate_extension(extension: &str) -> Result<&str, NativeJobError> {
    if extension.is_empty()
        || extension.len() > 32
        || !extension
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'_' | b'-'))
    {
        return Err(invalid_input("pinned character asset extension is invalid"));
    }
    Ok(extension)
}

fn archive_asset_path(
    asset: &Value,
    index: usize,
    extension: &str,
) -> Result<String, NativeJobError> {
    let asset_type = asset
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_input("CCv3 asset type must be a string"))?;
    let category = match asset_type {
        "emotion" | "background" | "user_icon" | "icon" => asset_type,
        _ => "other",
    };
    Ok(format!("assets/{category}/asset_{index}.{extension}"))
}

fn copy_verified_object(
    archive: &mut ZipWriter<BufWriter<File>>,
    mut source: File,
    expected_hash: &str,
    expected_size: i64,
    job: &JobControl,
) -> Result<u64, NativeJobError> {
    let expected_size = u64::try_from(expected_size)
        .map_err(|_| invalid_input("pinned character asset size is invalid"))?;
    let mut hasher = Sha256::new();
    let mut copied = 0_u64;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        if job.is_cancel_requested() {
            return Err(cancelled(
                "character CharX export cancelled while reading an asset",
            ));
        }
        let read = source.read(&mut buffer).map_err(io_error)?;
        if read == 0 {
            break;
        }
        archive.write_all(&buffer[..read]).map_err(io_error)?;
        hasher.update(&buffer[..read]);
        copied = copied
            .checked_add(read as u64)
            .ok_or_else(|| invalid_input("pinned character asset size overflowed"))?;
    }
    if copied != expected_size || hex::encode(hasher.finalize()) != expected_hash {
        return Err(NativeJobError::new(
            "hash-mismatch",
            "pinned character asset differs from its leased CAS identity",
        ));
    }
    Ok(copied)
}

fn write_module_overlay(
    archive: &mut ZipWriter<BufWriter<File>>,
    module: Value,
    job: &JobControl,
) -> Result<(), NativeJobError> {
    if job.is_cancel_requested() {
        return Err(cancelled(
            "character CharX export cancelled before module overlay",
        ));
    }
    let metadata = serde_json::to_vec_pretty(&json!({
        "module": module,
        "type": "risuModule",
    }))
    .map_err(|error| invalid_input(format!("module overlay is invalid: {error}")))?;
    let encoded: Vec<u8> = metadata
        .into_iter()
        .map(|byte| RPACK_MAP[byte as usize])
        .collect();
    let encoded_len = u32::try_from(encoded.len())
        .map_err(|_| invalid_input("module overlay exceeds RISUM V0 size"))?;
    archive
        .start_file(
            "module.risum",
            FileOptions::default().compression_method(CompressionMethod::Stored),
        )
        .map_err(zip_error)?;
    archive.write_all(&[111, 0]).map_err(io_error)?;
    archive
        .write_all(&encoded_len.to_le_bytes())
        .map_err(io_error)?;
    archive.write_all(&encoded).map_err(io_error)?;
    archive.write_all(&[0]).map_err(io_error)
}

fn write_card_metadata(
    archive: &mut ZipWriter<BufWriter<File>>,
    card: &Value,
    job: &JobControl,
) -> Result<(), NativeJobError> {
    if job.is_cancel_requested() {
        return Err(cancelled(
            "character CharX export cancelled before card metadata",
        ));
    }
    let metadata = serde_json::to_vec_pretty(card)
        .map_err(|error| invalid_input(format!("CCv3 metadata is invalid: {error}")))?;
    if metadata.len() > CARD_METADATA_LIMIT {
        return Err(invalid_input("CCv3 metadata exceeds the 8 MiB limit"));
    }
    archive
        .start_file(
            "card.json",
            FileOptions::default().compression_method(CompressionMethod::Deflated),
        )
        .map_err(zip_error)?;
    for chunk in metadata.chunks(COPY_BUFFER_BYTES) {
        if job.is_cancel_requested() {
            return Err(cancelled(
                "character CharX export cancelled while writing metadata",
            ));
        }
        archive.write_all(chunk).map_err(io_error)?;
    }
    Ok(())
}

fn finish_with_lease(
    outcome: Result<JobResultSummary, NativeJobError>,
    prepared: &PreparedRisuSaveExport,
    reader: RevisionReadLease,
) -> Result<JobResultSummary, NativeJobError> {
    match (outcome, prepared.release(reader)) {
        (Ok(mut result), Err(_)) => {
            result.warning_codes.push("cleanup-failed".to_owned());
            Ok(result)
        }
        (Ok(result), Ok(())) => Ok(result),
        (Err(error), Err(cleanup)) => Err(NativeJobError::new(
            "cleanup-failed",
            format!(
                "{}; character export lease release failed: {cleanup}",
                error.message
            ),
        )),
        (Err(error), Ok(())) => Err(error),
    }
}

fn destination_error(error: destination::DestinationWriteError) -> NativeJobError {
    match error {
        destination::DestinationWriteError::InvalidSource => {
            NativeJobError::new("invalid-source", "character CharX source is unavailable")
        }
        destination::DestinationWriteError::InvalidDestination => NativeJobError::new(
            "invalid-destination",
            "character CharX destination is invalid",
        ),
        destination::DestinationWriteError::Cancelled => {
            cancelled("character CharX export cancelled before destination replacement")
        }
        destination::DestinationWriteError::Io { operation, source } => {
            NativeJobError::new("destination-write-failed", format!("{operation}: {source}"))
        }
    }
}

fn store_error(error: StoreError) -> NativeJobError {
    match error {
        StoreError::RevisionConflict { .. } => {
            NativeJobError::new("revision-conflict", error.to_string())
        }
        StoreError::Validation { .. } => invalid_input(error.to_string()),
        StoreError::SnapshotReleased | StoreError::Store { .. } => {
            NativeJobError::new("store-error", error.to_string())
        }
    }
}

fn io_error(error: std::io::Error) -> NativeJobError {
    NativeJobError::new("store-error", error.to_string())
}

fn zip_error(error: zip::result::ZipError) -> NativeJobError {
    NativeJobError::new("store-error", error.to_string())
}

fn invalid_input(message: impl AsRef<str>) -> NativeJobError {
    NativeJobError::new("invalid-input", message)
}

fn cancelled(message: impl AsRef<str>) -> NativeJobError {
    NativeJobError::new("cancelled", message)
}

fn job_error(message: impl AsRef<str>) -> NativeJobError {
    NativeJobError::new("job-error", message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset_repository::{owner_manifest_codec, PayloadCas};
    use crate::native_file_jobs::charx::{inspect_charx_file, CharXInspection, CharXLimits};
    use crate::native_file_jobs::{JobKind, JobRegistry};
    use crate::persistent_store::{
        AssetAlias, AssetOwnerHead, AssetOwnerLocator, AssetRepositoryAuthorityState,
        PersistentStore,
    };
    use serde_json::json;
    use tempfile::TempDir;

    struct Fixture {
        directory: TempDir,
        store: PersistentStore,
        revision: i64,
        card: Value,
        module: Value,
        payload_hashes: Vec<String>,
    }

    fn alias(key: &str, payload: &[u8], extension: &str, cas: &PayloadCas) -> AssetAlias {
        let prepared = cas.prepare_bytes(payload).unwrap();
        AssetAlias {
            key: key.to_owned(),
            object_hash: Some(prepared.content_hash),
            kind: "asset".to_owned(),
            size: prepared.byte_size as i64,
            mime: "application/octet-stream".to_owned(),
            name: key.to_owned(),
            ext: extension.to_owned(),
            inlay_type: None,
            width: None,
            height: None,
            metadata: json!({}),
        }
    }

    fn fixture() -> Fixture {
        let directory = TempDir::new().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let shared = alias("assets/shared.bin", b"shared-original", "BIN", &cas);
        let emotion = alias("assets/emotion.webp", b"emotion-original", "WEBP", &cas);
        let portrait = alias("assets/portrait.png", b"portrait-original", "PNG", &cas);
        let cc = alias("assets/cc.dat", b"cc-original", "DAT", &cas);
        let manifest_bytes = owner_manifest_codec::encode_owner_manifest(&[
            owner_manifest_codec::OwnerManifestEntry {
                tuple: ["first".to_owned(), shared.key.clone(), shared.ext.clone()],
                payload_hash: Some(
                    hex::decode(shared.object_hash.as_ref().unwrap())
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
            },
            owner_manifest_codec::OwnerManifestEntry {
                tuple: ["first".to_owned(), shared.key.clone(), shared.ext.clone()],
                payload_hash: Some(
                    hex::decode(shared.object_hash.as_ref().unwrap())
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
            "chaId": "current-character",
            "name": "Current",
            "image": portrait.key.clone(),
            "ccAssets": [{
                "type": "x-custom",
                "uri": cc.key.clone(),
                "name": "custom",
                "ext": "DAT"
            }],
            "additionalAssets": [
                ["first", shared.key.clone(), "BIN"],
                ["first", shared.key.clone(), "BIN"]
            ],
            "emotionImages": [["happy", emotion.key.clone()]],
            "triggerscript": [{"comment": "trigger"}],
            "customscript": [{"comment": "regex"}],
            "globalLore": [{"comment": "lore"}],
            "chats": []
        });
        let staging = store.replace_begin().unwrap();
        store
            .replace_put_root(&staging.staging_id, &json!({}))
            .unwrap();
        store.replace_put_presets(&staging.staging_id, &[]).unwrap();
        store
            .replace_add_characters(&staging.staging_id, &[character])
            .unwrap();
        store
            .replace_put_asset_aliases(
                &staging.staging_id,
                &[
                    shared.clone(),
                    emotion.clone(),
                    portrait.clone(),
                    cc.clone(),
                ],
            )
            .unwrap();
        store
            .replace_put_asset_owner_heads(
                &staging.staging_id,
                &[AssetOwnerHead::present(
                    AssetOwnerLocator::CharacterAdditionalAssets {
                        character_id: "current-character".to_owned(),
                    },
                    manifest.content_hash,
                    2,
                )],
            )
            .unwrap();
        store
            .replace_put_asset_repository_authority(
                &staging.staging_id,
                &AssetRepositoryAuthorityState::V2 {
                    migration_id: "character-charx-test".to_owned(),
                    compatibility_hash: "ab".repeat(32),
                },
            )
            .unwrap();
        let revision = store
            .replace_commit(&staging.staging_id, Some(0))
            .unwrap()
            .revision;
        let card = json!({
            "spec": "chara_card_v3",
            "spec_version": "3.0",
            "data": {
                "name": "Current",
                "extensions": {"risuai": {}},
                "assets": [
                    {"type": "x-custom", "uri": cc.key.clone(), "name": "custom", "ext": "DAT"},
                    {"type": "x-risu-asset", "uri": shared.key.clone(), "name": "first", "ext": "BIN"},
                    {"type": "x-risu-asset", "uri": shared.key.clone(), "name": "first", "ext": "BIN"},
                    {"type": "emotion", "uri": emotion.key.clone(), "name": "happy", "ext": "png"},
                    {"type": "icon", "uri": "ccdefault:", "name": "main", "ext": "png"}
                ]
            }
        });
        let module = json!({
            "name": "Current Module",
            "description": "Module for Current",
            "id": "module-id",
            "trigger": [{"comment": "trigger"}],
            "regex": [{"comment": "regex"}],
            "lorebook": [{"comment": "lore"}]
        });
        Fixture {
            directory,
            store,
            revision,
            card,
            module,
            payload_hashes: vec![
                cc.object_hash.unwrap(),
                shared.object_hash.clone().unwrap(),
                shared.object_hash.unwrap(),
                emotion.object_hash.unwrap(),
                portrait.object_hash.unwrap(),
            ],
        }
    }

    #[test]
    fn native_charx_export_streams_the_exact_leased_asset_graph_in_order() {
        let mut fixture = fixture();
        let prepared = fixture
            .store
            .prepare_risu_save_export(fixture.revision)
            .unwrap();
        let owned = fixture.directory.path().join("owned");
        let chosen = fixture.directory.path().join("chosen");
        fs::create_dir(&owned).unwrap();
        fs::create_dir(&chosen).unwrap();
        let destination = chosen.join("current.charx");
        let job = JobRegistry::default()
            .create(JobKind::ExportCharacterCharx)
            .unwrap();

        let result = export_character_charx(
            prepared,
            "current-character",
            fixture.card,
            fixture.module,
            &owned,
            &destination,
            &job,
        )
        .unwrap();

        assert_eq!(result.revision, fixture.revision);
        assert_eq!(result.character_count, 1);
        let parsed_root = fixture.directory.path().join("parsed");
        fs::create_dir(&parsed_root).unwrap();
        let inspection = inspect_charx_file(
            &destination,
            "current.charx",
            &parsed_root,
            CharXLimits::default(),
            || false,
        )
        .unwrap();
        let CharXInspection::Card(parsed) = inspection else {
            panic!("exported file must be a CharX card")
        };
        let assets = parsed
            .payloads
            .iter()
            .filter(|payload| payload.original_name.starts_with("assets/"))
            .collect::<Vec<_>>();
        assert_eq!(
            assets
                .iter()
                .map(|payload| payload.sha256.clone())
                .collect::<Vec<_>>(),
            fixture.payload_hashes
        );
        assert_eq!(
            assets
                .iter()
                .map(|payload| payload.extension.clone().unwrap())
                .collect::<Vec<_>>(),
            ["DAT", "BIN", "BIN", "WEBP", "PNG"]
        );
        assert_eq!(
            parsed
                .asset_references
                .iter()
                .map(|reference| reference.order)
                .collect::<Vec<_>>(),
            [0, 1, 2, 3, 4]
        );
    }

    #[test]
    fn cancellation_preserves_an_existing_character_destination() {
        let mut fixture = fixture();
        let prepared = fixture
            .store
            .prepare_risu_save_export(fixture.revision)
            .unwrap();
        let owned = fixture.directory.path().join("owned");
        let chosen = fixture.directory.path().join("chosen");
        fs::create_dir(&owned).unwrap();
        fs::create_dir(&chosen).unwrap();
        let destination = chosen.join("current.charx");
        fs::write(&destination, b"previous CharX").unwrap();
        let job = JobRegistry::default()
            .create(JobKind::ExportCharacterCharx)
            .unwrap();
        job.request_cancel().unwrap();

        let error = export_character_charx(
            prepared,
            "current-character",
            fixture.card,
            fixture.module,
            &owned,
            &destination,
            &job,
        )
        .unwrap_err();

        assert_eq!(error.code, "cancelled");
        assert_eq!(fs::read(destination).unwrap(), b"previous CharX");
    }

    #[test]
    fn stale_character_revision_is_rejected_before_a_job_can_publish() {
        let mut fixture = fixture();
        let error = fixture
            .store
            .prepare_risu_save_export(fixture.revision + 1)
            .err()
            .unwrap();
        assert!(matches!(error, StoreError::RevisionConflict { .. }));
    }
}
