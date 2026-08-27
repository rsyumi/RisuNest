use super::charx::CharXLimits;
use super::{JobControl, JobPhase, JobProgress, JobResultSummary, NativeJobError};
use crate::asset_repository::{owner_manifest_codec::OwnerManifestEntry, PayloadCas};
use crate::persistent_store::export::{self, destination};
use crate::persistent_store::{PreparedRisuSaveExport, RevisionReadLease, StoreError};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::path::Path;
use uuid::Uuid;
use zip::write::FileOptions;
use zip::{CompressionMethod, ZipWriter};

const COPY_BUFFER_BYTES: usize = 64 * 1024;
const RPACK_MAP: &[u8; 512] = include_bytes!("../../../src/ts/rpack/rpack_map.bin");
const FALLBACK_PORTRAIT: &[u8] = include_bytes!("../../../public/none.webp");

enum EmbeddedAssetSource {
    Alias(String),
    ManifestOccurrence { key: String, hash: String },
    FallbackPortrait,
}

pub(crate) fn export_character_charx(
    mut prepared: PreparedRisuSaveExport,
    character_id: &str,
    card: Value,
    module: Value,
    owned_directory: &Path,
    handoff_directory: &Path,
    destination_path: Option<&Path>,
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
        handoff_directory,
        destination_path,
        job,
        CharXLimits::default(),
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
    handoff_directory: &Path,
    destination_path: Option<&Path>,
    job: &JobControl,
    limits: CharXLimits,
) -> Result<JobResultSummary, NativeJobError> {
    if job.is_cancel_requested() {
        return Err(cancelled(
            "character CharX export cancelled before encoding",
        ));
    }
    job.start(JobPhase::WritingExport).map_err(job_error)?;
    let projected = export::projected_character(
        &reader.connection,
        &prepared.snapshots_dir,
        &reader.target,
        character_id,
    )
    .map_err(store_error)?;
    let character = projected.value;
    let additional_asset_entries = projected.additional_asset_entries;
    validate_character_identity(&character, character_id, &card)?;
    validate_module_overlay(&character, &card, &module)?;
    let assets = card_assets_mut(&mut card)?;
    let asset_count = assets.len();
    let embedded_asset_count = assets.iter().try_fold(0_usize, |count, asset| {
        asset_is_embedded(asset).and_then(|embedded| {
            if embedded {
                count
                    .checked_add(1)
                    .ok_or_else(|| invalid_input("character CharX entry count overflowed"))
            } else {
                Ok(count)
            }
        })
    })?;
    let entry_count = embedded_asset_count
        .checked_add(2)
        .ok_or_else(|| invalid_input("character CharX entry count overflowed"))?;
    if entry_count > limits.max_entries {
        return Err(invalid_input(format!(
            "character CharX has {entry_count} entries, limit is {}",
            limits.max_entries
        )));
    }
    let total_items = u64::try_from(
        asset_count
            .checked_add(2)
            .ok_or_else(|| invalid_input("character CharX item count overflowed"))?,
    )
    .map_err(|_| invalid_input("character CharX entry count is invalid"))?;
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
    let mut decoded_bytes = 0_u64;
    let cc_asset_count = character
        .get("ccAssets")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let additional_asset_count = character
        .get("additionalAssets")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    if let Some(entries) = &additional_asset_entries {
        if entries.len() != additional_asset_count {
            return Err(invalid_input(
                "pinned character owner manifest occurrence count changed",
            ));
        }
    }

    let assets = card_assets_mut(&mut card)?;
    for (index, asset) in assets.iter_mut().enumerate() {
        if job.is_cancel_requested() {
            return Err(cancelled(
                "character CharX export cancelled while writing assets",
            ));
        }
        let occurrence = index
            .checked_sub(cc_asset_count)
            .filter(|offset| *offset < additional_asset_count)
            .and_then(|offset| {
                additional_asset_entries
                    .as_ref()
                    .and_then(|entries| entries.get(offset))
            });
        let Some(source_identity) = embedded_asset_source(asset, &character, occurrence)? else {
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
        let (hash, size, key) = match &source_identity {
            EmbeddedAssetSource::Alias(key) => {
                let alias = export::pinned_asset_alias(&reader.connection, &reader.target, key)
                    .map_err(store_error)?;
                let hash = alias.object_hash.ok_or_else(|| {
                    invalid_input(format!(
                        "pinned character asset has no native payload: {key}"
                    ))
                })?;
                let size = u64::try_from(alias.size)
                    .map_err(|_| invalid_input("pinned character asset size is invalid"))?;
                (Some(hash), size, key.as_str())
            }
            EmbeddedAssetSource::ManifestOccurrence { key, hash } => {
                let size = repository
                    .stat_object(hash)
                    .map_err(io_error)?
                    .ok_or_else(|| {
                        invalid_input(format!("pinned character asset is missing: {key}"))
                    })?;
                (Some(hash.clone()), size, key.as_str())
            }
            EmbeddedAssetSource::FallbackPortrait => (
                None,
                FALLBACK_PORTRAIT.len() as u64,
                "built-in fallback portrait",
            ),
        };
        reserve_decoded_entry(&mut decoded_bytes, size, "character asset", limits)?;
        let extension = asset
            .get("ext")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_input("CCv3 asset extension must be a string"))?;
        let extension = validate_extension(extension)?.to_owned();
        let archive_path = archive_asset_path(asset, index, &extension)?;
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
            .insert("ext".to_owned(), Value::String(extension));

        archive
            .start_file(
                archive_path,
                FileOptions::default()
                    .compression_method(CompressionMethod::Stored)
                    .large_file(size >= u64::from(u32::MAX)),
            )
            .map_err(zip_error)?;
        let copied = if let Some(hash) = hash {
            let source_file = repository
                .open_object(&hash)
                .map_err(io_error)?
                .ok_or_else(|| {
                    invalid_input(format!("pinned character asset is missing: {key}"))
                })?;
            copy_verified_object(&mut archive, source_file, &hash, size, job)?
        } else {
            copy_fallback_portrait(&mut archive, job)?
        };
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

    write_module_overlay(&mut archive, module, job, &mut decoded_bytes, limits)?;
    completed_items += 1;
    job.set_progress(JobProgress {
        completed_bytes,
        total_bytes: None,
        completed_items,
        total_items: Some(total_items),
    })
    .map_err(job_error)?;
    write_card_metadata(&mut archive, &card, job, &mut decoded_bytes, limits)?;
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
    let (destination_root, destination_path, handoff_path) = match destination_path {
        Some(destination_path) => (
            destination_path.parent().ok_or_else(|| {
                NativeJobError::new(
                    "invalid-destination",
                    "character CharX destination directory is unavailable",
                )
            })?,
            destination_path.to_owned(),
            None,
        ),
        None => {
            fs::create_dir_all(handoff_directory).map_err(io_error)?;
            let path = handoff_directory.join(format!("risu-charx-{}.charx", Uuid::new_v4()));
            (
                handoff_directory,
                path.clone(),
                Some(path.to_string_lossy().into_owned()),
            )
        }
    };
    let phase_failure = RefCell::new(None);
    let published = destination::write_charx_destination_controlled(
        owned_directory,
        &source,
        destination_root,
        &destination_path,
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
        handoff_path,
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

fn asset_is_embedded(asset: &Value) -> Result<bool, NativeJobError> {
    let uri = asset
        .as_object()
        .and_then(|asset| asset.get("uri"))
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_input("CCv3 asset URI must be a string"))?;
    if uri.starts_with("embeded://") {
        return Err(invalid_input(
            "live character metadata cannot reference an unleased embedded path",
        ));
    }
    if uri.is_empty() {
        return Err(invalid_input("CCv3 asset URI must not be empty"));
    }
    Ok(!uri.starts_with("http://") && !uri.starts_with("https://"))
}

fn embedded_asset_source(
    asset: &Value,
    character: &Value,
    occurrence: Option<&OwnerManifestEntry>,
) -> Result<Option<EmbeddedAssetSource>, NativeJobError> {
    let asset = asset
        .as_object()
        .ok_or_else(|| invalid_input("CCv3 asset must be an object"))?;
    let uri = asset
        .get("uri")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_input("CCv3 asset URI must be a string"))?;
    if uri == "ccdefault:" {
        return Ok(character
            .get("image")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .map(EmbeddedAssetSource::Alias)
            .or(Some(EmbeddedAssetSource::FallbackPortrait)));
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
    if let Some(occurrence) = occurrence {
        let hash = occurrence.payload_hash.ok_or_else(|| {
            invalid_input(format!(
                "pinned character owner occurrence has no native payload: {uri}"
            ))
        })?;
        return Ok(Some(EmbeddedAssetSource::ManifestOccurrence {
            key: uri.to_owned(),
            hash: hex::encode(hash),
        }));
    }
    Ok(Some(EmbeddedAssetSource::Alias(uri.to_owned())))
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

fn reserve_decoded_entry(
    total: &mut u64,
    entry_bytes: u64,
    description: &str,
    limits: CharXLimits,
) -> Result<(), NativeJobError> {
    if entry_bytes > limits.max_entry_decoded_bytes {
        return Err(invalid_input(format!(
            "{description} exceeds the CharX per-entry limit"
        )));
    }
    let next = total
        .checked_add(entry_bytes)
        .ok_or_else(|| invalid_input("character CharX decoded size overflowed"))?;
    if next > limits.max_total_decoded_bytes {
        return Err(invalid_input(
            "character CharX exceeds the aggregate decoded size limit",
        ));
    }
    *total = next;
    Ok(())
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
    expected_size: u64,
    job: &JobControl,
) -> Result<u64, NativeJobError> {
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

fn copy_fallback_portrait(
    archive: &mut ZipWriter<BufWriter<File>>,
    job: &JobControl,
) -> Result<u64, NativeJobError> {
    let mut copied = 0_u64;
    for chunk in FALLBACK_PORTRAIT.chunks(COPY_BUFFER_BYTES) {
        if job.is_cancel_requested() {
            return Err(cancelled(
                "character CharX export cancelled while reading the fallback portrait",
            ));
        }
        archive.write_all(chunk).map_err(io_error)?;
        copied = copied
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| invalid_input("fallback portrait size overflowed"))?;
    }
    Ok(copied)
}

fn write_module_overlay(
    archive: &mut ZipWriter<BufWriter<File>>,
    module: Value,
    job: &JobControl,
    decoded_bytes: &mut u64,
    limits: CharXLimits,
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
    let entry_bytes = u64::try_from(encoded.len())
        .ok()
        .and_then(|bytes| bytes.checked_add(7))
        .ok_or_else(|| invalid_input("module overlay size overflowed"))?;
    reserve_decoded_entry(decoded_bytes, entry_bytes, "module overlay", limits)?;
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
    decoded_bytes: &mut u64,
    limits: CharXLimits,
) -> Result<(), NativeJobError> {
    if job.is_cancel_requested() {
        return Err(cancelled(
            "character CharX export cancelled before card metadata",
        ));
    }
    let metadata = serde_json::to_vec_pretty(card)
        .map_err(|error| invalid_input(format!("CCv3 metadata is invalid: {error}")))?;
    if metadata.len() as u64 > limits.max_metadata_bytes {
        return Err(invalid_input("CCv3 metadata exceeds the 8 MiB limit"));
    }
    reserve_decoded_entry(
        decoded_bytes,
        metadata.len() as u64,
        "CCv3 metadata",
        limits,
    )?;
    archive
        .start_file(
            "card.json",
            FileOptions::default().compression_method(CompressionMethod::Stored),
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
    use zip::ZipArchive;

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
        fixture_with_portrait(true)
    }

    fn fixture_with_portrait(has_portrait: bool) -> Fixture {
        let directory = TempDir::new().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let shared = alias("assets/shared.bin", b"shared-original", "BIN", &cas);
        let second_occurrence = cas.prepare_bytes(b"second-occurrence").unwrap();
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
                tuple: ["second".to_owned(), shared.key.clone(), "bIn".to_owned()],
                payload_hash: Some(
                    hex::decode(&second_occurrence.content_hash)
                        .unwrap()
                        .try_into()
                        .unwrap(),
                ),
            },
        ])
        .unwrap();
        let manifest = cas.prepare_bytes(&manifest_bytes).unwrap();
        let portrait_key = if has_portrait {
            portrait.key.clone()
        } else {
            String::new()
        };
        let character = json!({
            "type": "character",
            "chaId": "current-character",
            "name": "Current",
            "image": portrait_key,
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
        let mut aliases = vec![shared.clone(), emotion.clone(), cc.clone()];
        if has_portrait {
            aliases.push(portrait.clone());
        }
        store
            .replace_put_asset_aliases(&staging.staging_id, &aliases)
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
                    {"type": "x-risu-asset", "uri": shared.key.clone(), "name": "second", "ext": "bIn"},
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
                second_occurrence.content_hash,
                emotion.object_hash.unwrap(),
                if has_portrait {
                    portrait.object_hash.unwrap()
                } else {
                    hex::encode(Sha256::digest(FALLBACK_PORTRAIT))
                },
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
            &fixture.directory.path().join("handoffs"),
            Some(&destination),
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
            ["DAT", "BIN", "bIn", "png", "png"]
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
            &fixture.directory.path().join("handoffs"),
            Some(&destination),
            &job,
        )
        .unwrap_err();

        assert_eq!(error.code, "cancelled");
        assert_eq!(fs::read(destination).unwrap(), b"previous CharX");
    }

    #[test]
    fn image_less_character_reimports_with_the_exact_built_in_fallback_portrait() {
        let mut fixture = fixture_with_portrait(false);
        let prepared = fixture
            .store
            .prepare_risu_save_export(fixture.revision)
            .unwrap();
        let owned = fixture.directory.path().join("owned");
        let chosen = fixture.directory.path().join("chosen");
        fs::create_dir(&owned).unwrap();
        fs::create_dir(&chosen).unwrap();
        let destination = chosen.join("image-less.charx");
        let job = JobRegistry::default()
            .create(JobKind::ExportCharacterCharx)
            .unwrap();

        export_character_charx(
            prepared,
            "current-character",
            fixture.card,
            fixture.module,
            &owned,
            &fixture.directory.path().join("handoffs"),
            Some(&destination),
            &job,
        )
        .unwrap();

        let parsed_root = fixture.directory.path().join("parsed-image-less");
        fs::create_dir(&parsed_root).unwrap();
        let CharXInspection::Card(parsed) = inspect_charx_file(
            &destination,
            "image-less.charx",
            &parsed_root,
            CharXLimits::default(),
            || false,
        )
        .unwrap() else {
            panic!("exported file must be a CharX card")
        };
        let portrait = parsed
            .payloads
            .iter()
            .find(|payload| payload.original_name.starts_with("assets/icon/"))
            .unwrap();
        assert_eq!(
            portrait.sha256,
            hex::encode(Sha256::digest(FALLBACK_PORTRAIT))
        );
        assert_eq!(fs::read(&portrait.staged_path).unwrap(), FALLBACK_PORTRAIT);
    }

    #[test]
    fn highly_compressible_card_metadata_is_stored_and_roundtrips() {
        let mut fixture = fixture();
        let repeated = "x".repeat(2 * 1024 * 1024);
        fixture.card["data"]["description"] = Value::String(repeated.clone());
        let prepared = fixture
            .store
            .prepare_risu_save_export(fixture.revision)
            .unwrap();
        let owned = fixture.directory.path().join("owned");
        let chosen = fixture.directory.path().join("chosen");
        fs::create_dir(&owned).unwrap();
        fs::create_dir(&chosen).unwrap();
        let destination = chosen.join("compressible.charx");
        let job = JobRegistry::default()
            .create(JobKind::ExportCharacterCharx)
            .unwrap();

        export_character_charx(
            prepared,
            "current-character",
            fixture.card,
            fixture.module,
            &owned,
            &fixture.directory.path().join("handoffs"),
            Some(&destination),
            &job,
        )
        .unwrap();

        let mut zip = ZipArchive::new(File::open(&destination).unwrap()).unwrap();
        assert_eq!(
            zip.by_name("card.json").unwrap().compression(),
            CompressionMethod::Stored
        );
        drop(zip);
        let parsed_root = fixture.directory.path().join("parsed-compressible");
        fs::create_dir(&parsed_root).unwrap();
        let CharXInspection::Card(parsed) = inspect_charx_file(
            &destination,
            "compressible.charx",
            &parsed_root,
            CharXLimits::default(),
            || false,
        )
        .unwrap() else {
            panic!("exported file must be a CharX card")
        };
        let card: Value = serde_json::from_str(&parsed.card_json).unwrap();
        assert_eq!(card["data"]["description"], repeated);
    }

    #[test]
    fn portable_character_charx_export_returns_an_app_owned_handoff() {
        let mut fixture = fixture();
        let prepared = fixture
            .store
            .prepare_risu_save_export(fixture.revision)
            .unwrap();
        let owned = fixture.directory.path().join("owned");
        let handoffs = fixture.directory.path().join("handoffs");
        fs::create_dir(&owned).unwrap();
        let job = JobRegistry::default()
            .create(JobKind::ExportCharacterCharx)
            .unwrap();

        let result = export_character_charx(
            prepared,
            "current-character",
            fixture.card,
            fixture.module,
            &owned,
            &handoffs,
            None,
            &job,
        )
        .unwrap();

        let handoff = result.handoff_path.map(std::path::PathBuf::from).unwrap();
        assert_eq!(handoff.parent(), Some(handoffs.as_path()));
        assert!(handoff.is_file());
        assert_eq!(fs::metadata(handoff).unwrap().len(), result.source_bytes);
    }

    #[test]
    fn native_charx_export_rejects_accepted_limits_before_destination_publication() {
        let base = CharXLimits::default();
        for (limits, expected_message) in [
            (
                CharXLimits {
                    max_entries: 6,
                    ..base
                },
                "entries",
            ),
            (
                CharXLimits {
                    max_entry_decoded_bytes: 10,
                    ..base
                },
                "per-entry",
            ),
            (
                CharXLimits {
                    max_total_decoded_bytes: 25,
                    ..base
                },
                "aggregate",
            ),
            (
                CharXLimits {
                    max_metadata_bytes: 1,
                    ..base
                },
                "metadata",
            ),
        ] {
            let mut fixture = fixture();
            let mut prepared = fixture
                .store
                .prepare_risu_save_export(fixture.revision)
                .unwrap();
            let reader = prepared.take_reader().unwrap();
            let owned = fixture.directory.path().join("owned");
            let chosen = fixture.directory.path().join("chosen");
            fs::create_dir(&owned).unwrap();
            fs::create_dir(&chosen).unwrap();
            let destination = chosen.join("current.charx");
            fs::write(&destination, b"previous CharX").unwrap();
            let job = JobRegistry::default()
                .create(JobKind::ExportCharacterCharx)
                .unwrap();

            let error = export_character_charx_with_reader(
                &prepared,
                &reader,
                "current-character",
                fixture.card,
                fixture.module,
                &owned,
                &fixture.directory.path().join("handoffs"),
                Some(&destination),
                &job,
                limits,
            )
            .unwrap_err();

            assert_eq!(error.code, "invalid-input");
            assert!(
                error.message.contains(expected_message),
                "{}",
                error.message
            );
            assert_eq!(fs::read(destination).unwrap(), b"previous CharX");
            prepared.release(reader).unwrap();
        }
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
