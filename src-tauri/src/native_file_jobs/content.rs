use super::{
    charx::{
        inspect_charx_opened_file, CharXContainerKind, CharXInspection, CharXLimits,
        CharXParseError, CharXParseErrorCode, ParsedCharXDescriptor,
    },
    content_cancelled, open_regular_file_no_follow, ImportCounts, JobControl, JobDetail, JobPhase,
    JobProgress, JobStage, NativeJobError, OpenedJobSource, PreparedContent, PreparedContentAsset,
    PreparedContentFormat, PreparedContentModule, PreparedContentOwnerHead, StageUnit,
};
use crate::asset_repository::job_pins::{
    CasJobKind, CasObjectRole, CasReleaseOutcome, DurableCasJob,
};
use crate::asset_repository::{PayloadCas, PreparedPayload};
use crate::asset_repository::import_payloads::{prepare_import_payloads, ImportPayload, ImportPinning};
use crate::import_export_jobs::png_card::{
    parse_png_card, PngCardError, PngCardLimits, PngCardParseResult, StagedPngPayload,
};
use crate::import_export_jobs::{
    classify_content, parse_json_card, parse_risum, ContentKind, FormatError, FormatErrorKind,
    ImportLimits, JobStaging, JsonCardPayload, ParsedJsonCard,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
#[cfg(test)]
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const COPY_BUFFER_BYTES: usize = 64 * 1024;
pub(super) const JSON_CARD_MAX_METADATA_BYTES: usize =
    crate::import_export_jobs::MAX_CONTENT_METADATA_BYTES;
// Metadata bytes already bound memory; large lorebooks must round-trip through
// our own module.risum overlay just like standalone modules.
const MAX_MODULE_OVERLAY_ITEMS: usize = 50_000;
const ZIP_LOCAL_FILE_HEADER_BYTES: u64 = 30;
const ZIP_LOCAL_VARIABLE_HEADER_MAX_BYTES: u64 = u16::MAX as u64 * 2;
const ZIP_DATA_DESCRIPTOR_MAX_BYTES: u64 = 24;
const ZIP_FOOTER_MAX_BYTES: u64 = 22 + 20 + 56 + u16::MAX as u64;

struct CancellableReader<'a, R> {
    inner: R,
    job: &'a JobControl,
}

impl<R: Read> Read for CancellableReader<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.job.is_cancel_requested() {
            return Err(io::Error::other("content preparation was cancelled"));
        }
        self.inner.read(buffer)
    }
}

#[cfg(test)]
pub(super) fn spool_opened_source(
    source: &mut OpenedJobSource,
    destination: &Path,
    is_cancelled: &impl Fn() -> bool,
) -> Result<(), NativeJobError> {
    if is_cancelled() {
        return Err(content_cancelled());
    }
    source
        .file
        .seek(SeekFrom::Start(0))
        .map_err(|error| NativeJobError::new("invalid-source", error.to_string()))?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|error| NativeJobError::new("invalid-source", error.to_string()))?;
    let mut remove_partial = true;
    let result = (|| {
        let mut copied = 0_u64;
        let mut buffer = [0_u8; COPY_BUFFER_BYTES];
        while copied < source.total_bytes {
            if is_cancelled() {
                return Err(content_cancelled());
            }
            let remaining = (source.total_bytes - copied).min(buffer.len() as u64) as usize;
            let read = source
                .file
                .read(&mut buffer[..remaining])
                .map_err(|error| NativeJobError::new("invalid-source", error.to_string()))?;
            if read == 0 {
                return Err(NativeJobError::new(
                    "invalid-source",
                    "source ended before its opened length",
                ));
            }
            output
                .write_all(&buffer[..read])
                .map_err(|error| NativeJobError::new("invalid-source", error.to_string()))?;
            copied += read as u64;
        }
        if is_cancelled() {
            return Err(content_cancelled());
        }
        let mut extra = [0_u8; 1];
        if source
            .file
            .read(&mut extra)
            .map_err(|error| NativeJobError::new("invalid-source", error.to_string()))?
            != 0
        {
            return Err(NativeJobError::new(
                "invalid-source",
                "source grew after it was opened",
            ));
        }
        output
            .flush()
            .map_err(|error| NativeJobError::new("invalid-source", error.to_string()))?;
        output
            .sync_all()
            .map_err(|error| NativeJobError::new("invalid-source", error.to_string()))?;
        remove_partial = false;
        Ok(())
    })();
    drop(output);
    if remove_partial {
        let _ = fs::remove_file(destination);
    }
    result
}

pub(super) fn max_charx_spool_bytes() -> u64 {
    let limits = CharXLimits::default();
    let per_entry_overhead = ZIP_LOCAL_FILE_HEADER_BYTES
        .checked_add(ZIP_LOCAL_VARIABLE_HEADER_MAX_BYTES)
        .and_then(|value| value.checked_add(ZIP_DATA_DESCRIPTOR_MAX_BYTES))
        .expect("CharX local ZIP overhead must fit in u64");
    let local_overhead = (limits.max_entries as u64)
        .checked_mul(per_entry_overhead)
        .expect("CharX local ZIP overhead must fit in u64");
    limits
        .max_total_decoded_bytes
        .checked_add(limits.max_directory_bytes)
        .and_then(|value| value.checked_add(local_overhead))
        .and_then(|value| value.checked_add(ZIP_FOOTER_MAX_BYTES))
        .expect("CharX raw source limit must fit in u64")
}

#[cfg(test)]
pub(super) fn spool_charx_source(
    source: &mut OpenedJobSource,
    destination: &Path,
    is_cancelled: &impl Fn() -> bool,
) -> Result<(), NativeJobError> {
    if is_cancelled() {
        return Err(content_cancelled());
    }
    if source.total_bytes > max_charx_spool_bytes() {
        return Err(NativeJobError::too_large("card", "CharX source exceeds its raw container limit",
        ));
    }
    spool_opened_source(source, destination, is_cancelled)
}

pub(super) fn prepare_content(
    mut source: OpenedJobSource,
    display_name: &str,
    owned_directory: &Path,
    repository_root: &Path,
    job: &JobControl,
) -> Result<PreparedContent, NativeJobError> {
    if job.is_cancel_requested() {
        return Err(content_cancelled());
    }
    job.start(JobPhase::ReadingSource).map_err(|error| {
        if job.is_cancel_requested() {
            content_cancelled()
        } else {
            NativeJobError::new("store-error", error)
        }
    })?;
    #[cfg(test)]
    if let Some(gate) = &job.content_preparation_gate {
        gate.entered.send(()).unwrap();
        gate.resume.lock().unwrap().recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    }
    job.set_progress(JobProgress {
        completed_bytes: 0,
        total_bytes: Some(source.total_bytes),
        completed_items: 0,
        total_items: None,
    })
    .map_err(|error| NativeJobError::new("store-error", error))?;

    report_content_progress(job, JobStage::ReadingArchive, 0, None)?;
    let classifier_limits = content_classification_limits();
    let kind = classify_content(display_name, &mut source.file, &classifier_limits, &|| {
        job.is_cancel_requested()
    })
    .map_err(native_format_error)?;
    let supported = matches!(
        kind,
        ContentKind::JsonCard
            | ContentKind::PngCard
            | ContentKind::CharxCard
            | ContentKind::AppendedCharxJpeg
            | ContentKind::RisuModule
    );
    if !supported {
        return match kind {
            ContentKind::JpegAsset => Err(NativeJobError::new(
                "unsupported-without-destination",
                "ordinary JPEG import requires an asset destination",
            )),
            ContentKind::Unknown => Err(NativeJobError::new(
                "invalid-input",
                "content preparation accepts JSON, PNG, CharX, and RISUM content only",
            )),
            ContentKind::RescueArchive => Err(NativeJobError::new(
                "rescue-format-not-restorable",
                "RisuNest rescue archives cannot be imported or restored",
            )),
            _ => unreachable!("supported content kinds were handled above"),
        };
    }
    let mut cas_session = begin_content_cas_session(repository_root, job)?;
    let cas = match open_payload_cas(repository_root) {
        Ok(cas) => cas,
        Err(error) => return Err(abort_failed_content_session(cas_session, error)),
    };
    let prepared = match kind {
        ContentKind::JsonCard => prepare_json_content(
            &mut source,
            owned_directory,
            &content_import_limits(),
            &cas,
            &mut cas_session,
            job,
        ),
        ContentKind::PngCard => prepare_png_content(&mut source, owned_directory, &cas, &mut cas_session, job),
        ContentKind::CharxCard | ContentKind::AppendedCharxJpeg => {
            if source.total_bytes > max_charx_spool_bytes() {
                return Err(abort_failed_content_session(
                    cas_session,
                    NativeJobError::too_large("card", "CharX source exceeds its raw container limit",
                    ),
                ));
            }
            prepare_charx_content(&mut source, display_name, owned_directory, &cas, &mut cas_session, job)
        }
        ContentKind::RisuModule => {
            prepare_risum_content(&mut source, owned_directory, &cas, &mut cas_session, job)
        }
        _ => unreachable!("unsupported content kinds returned before opening a CAS session"),
    };
    let mut prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => return Err(abort_failed_content_session(cas_session, error)),
    };
    let item_count = prepared.assets.len() as u64;
    report_content_progress(
        job,
        JobStage::PreparingAttachments,
        item_count,
        Some(item_count),
    )?;
    if let Err(error) = job.set_progress(JobProgress {
        completed_bytes: source.total_bytes,
        total_bytes: Some(source.total_bytes),
        completed_items: item_count,
        total_items: Some(item_count),
    }) {
        let error = if job.is_cancel_requested() {
            content_cancelled()
        } else {
            NativeJobError::new("store-error", error)
        };
        return Err(abort_failed_content_session(cas_session, error));
    }
    prepared.cas_session_id = job.id();
    Ok(prepared)
}

fn prepare_png_content(
    source: &mut OpenedJobSource,
    owned_directory: &Path,
    cas: &PayloadCas,
    cas_session: &mut DurableCasJob,
    job: &JobControl,
) -> Result<PreparedContent, NativeJobError> {
    source
        .file
        .seek(SeekFrom::Start(0))
        .map_err(|error| NativeJobError::new("invalid-source", error.to_string()))?;
    let parsed = parse_png_card(
        &mut source.file,
        owned_directory,
        PngCardLimits::default(),
        || job.is_cancel_requested(),
    )
    .map_err(native_png_error)?;
    let PngCardParseResult {
        chara,
        ccv3,
        base_image,
        embedded_assets,
    } = parsed;
    validate_png_asset_references(&embedded_assets)?;

    let embedded_tokens = embedded_assets
        .iter()
        .filter_map(|payload| payload.asset_reference.as_deref())
        .collect::<HashSet<_>>();
    let portrait_token = collision_safe_png_portrait_token(&embedded_tokens);
    let inputs = std::iter::once(&base_image).chain(embedded_assets.iter()).map(|payload| ImportPayload {
        path: owned_directory.join(&payload.staged_id),
        hash: payload.sha256.clone(),
        size: payload.byte_size,
        sync_required: false,
        progress_items: 1,
    }).collect::<Vec<_>>();
    report_content_progress(job, JobStage::PreparingAttachments, 0, Some(inputs.len() as u64))?;
    let promoted = prepare_content_payloads(cas, cas_session, &inputs, ImportPinning::AfterContentMapping, job)?;
    let portrait = promoted_png_asset(&promoted[0], &portrait_token, "image/png");
    let portrait_logical_id = portrait.logical_id.clone();
    let mut assets = Vec::with_capacity(promoted.len());
    assets.push(portrait);
    for (payload, promoted) in embedded_assets.iter().zip(&promoted[1..]) {
        let reference = payload.asset_reference.as_deref()
            .expect("PNG asset references were validated before promotion");
        assets.push(promoted_png_asset(promoted, reference, ""));
    }

    let mut metadata = serde_json::Map::new();
    if let Some(chara) = chara {
        metadata.insert("chara".to_owned(), Value::String(chara));
    }
    if let Some(ccv3) = ccv3 {
        metadata.insert("ccv3".to_owned(), Value::String(ccv3));
    }
    Ok(PreparedContent {
        format: PreparedContentFormat::PngCard,
        metadata: Value::Object(metadata),
        assets,
        cas_session_id: String::new(),
        portrait_logical_id: Some(portrait_logical_id),
        module: None,
        owner_head: None,
    })
}

fn validate_png_asset_references(
    embedded_assets: &[StagedPngPayload],
) -> Result<(), NativeJobError> {
    if embedded_assets
        .iter()
        .any(|payload| payload.asset_reference.as_deref().is_none_or(str::is_empty))
    {
        return Err(NativeJobError::new(
            "invalid-input",
            "PNG embedded asset reference must not be empty",
        ));
    }
    Ok(())
}

fn collision_safe_png_portrait_token(embedded_tokens: &HashSet<&str>) -> String {
    const BASE: &str = "native-png-portrait";
    if !embedded_tokens.contains(BASE) {
        return BASE.to_owned();
    }
    for suffix in 1_u64.. {
        let candidate = format!("{BASE}-{suffix}");
        if !embedded_tokens.contains(candidate.as_str()) {
            return candidate;
        }
    }
    unreachable!("the finite PNG asset set cannot exhaust portrait tokens")
}

fn promoted_png_asset(payload: &PreparedPayload, token: &str, mime: &str) -> PreparedContentAsset {
    let object_hash = payload.content_hash.clone();
    PreparedContentAsset {
        reference_key: token.to_owned(),
        token: token.to_owned(),
        position: None,
        logical_id: format!("assets/{object_hash}.png"),
        name: format!("{object_hash}.png"),
        object_hash,
        byte_size: payload.byte_size,
        mime: mime.to_owned(),
        ext: "png".to_owned(),
    }
}

fn prepare_json_content(
    source: &mut OpenedJobSource,
    owned_directory: &Path,
    limits: &ImportLimits,
    cas: &PayloadCas,
    cas_session: &mut DurableCasJob,
    job: &JobControl,
) -> Result<PreparedContent, NativeJobError> {
    source
        .file
        .seek(SeekFrom::Start(0))
        .map_err(|error| NativeJobError::new("invalid-source", error.to_string()))?;
    let staging = JobStaging::open(owned_directory).map_err(native_format_error)?;
    let parsed = parse_json_card(&mut source.file, &staging, limits, &|| {
        job.is_cancel_requested()
    })
    .map_err(native_format_error)?;
    let ParsedJsonCard { metadata, payloads } = parsed;
    report_content_progress(job, JobStage::PreparingAttachments, 0, Some(payloads.len() as u64))?;
    let assets = promote_json_assets(&metadata, payloads, owned_directory, cas, cas_session, job)?;
    Ok(PreparedContent {
        format: PreparedContentFormat::JsonCard,
        metadata,
        assets,
        cas_session_id: String::new(),
        portrait_logical_id: None,
        module: None,
        owner_head: None,
    })
}

fn prepare_charx_content(
    source: &mut OpenedJobSource,
    display_name: &str,
    owned_directory: &Path,
    cas: &PayloadCas,
    cas_session: &mut DurableCasJob,
    job: &JobControl,
) -> Result<PreparedContent, NativeJobError> {
    let source_metadata = source
        .file
        .metadata()
        .map_err(|e| NativeJobError::new("invalid-source", e.to_string()))?;
    let inspection = inspect_charx_opened_file(
        &source.file,
        display_name,
        owned_directory,
        CharXLimits::default(),
        || job.is_cancel_requested(),
    )
    .map_err(native_charx_error)?;
    let descriptor = match inspection {
        CharXInspection::Card(descriptor) => descriptor,
        CharXInspection::OrdinaryJpegAsset(_) => {
            return Err(NativeJobError::new(
                "unsupported-without-destination",
                "ordinary JPEG import requires an asset destination",
            ));
        }
    };
    let module = parse_root_module_overlay(&descriptor, owned_directory, job)?;
    let mut metadata: Value = serde_json::from_str(&descriptor.card_json).map_err(|error| {
        NativeJobError::new(
            "invalid-input",
            format!("CharX card metadata is invalid: {error}"),
        )
    })?;
    rewrite_archive_asset_occurrences(&descriptor, &mut metadata)?;
    let staging = JobStaging::open(owned_directory).map_err(native_format_error)?;
    let serialized_metadata = serde_json::to_vec(&metadata).map_err(|error| {
        NativeJobError::new(
            "invalid-input",
            format!("CharX card metadata cannot be staged: {error}"),
        )
    })?;
    let parsed = parse_json_card(
        &mut std::io::Cursor::new(serialized_metadata),
        &staging,
        &content_import_limits(),
        &|| job.is_cancel_requested(),
    )
    .map_err(native_format_error)?;
    let ParsedJsonCard { metadata, payloads } = parsed;
    let total = descriptor.asset_references.len() as u64
        + payloads.len() as u64
        + u64::from(matches!(
            descriptor.container_kind,
            CharXContainerKind::AppendedCharXJpeg
        ));
    report_content_progress(job, JobStage::PreparingAttachments, 0, Some(total))?;
    let mut assets = promote_archive_asset_occurrences(&descriptor, cas, cas_session, job)?;
    assets.extend(promote_json_assets(
        &metadata,
        payloads,
        owned_directory,
        cas,
        cas_session,
        job,
    )?);
    let portrait_logical_id = match descriptor.container_kind {
        CharXContainerKind::CharX => None,
        CharXContainerKind::AppendedCharXJpeg => {
            let portrait = promote_jpeg_prefix(source, descriptor.archive_offset, cas, job)?;
            let logical_id = portrait.logical_id.clone();
            advance_content_assets(job, 1)?;
            assets.push(portrait);
            Some(logical_id)
        }
    };
    let format = match descriptor.container_kind {
        CharXContainerKind::CharX => PreparedContentFormat::CharxCard,
        CharXContainerKind::AppendedCharXJpeg => PreparedContentFormat::AppendedCharxJpeg,
    };
    let after = source
        .file
        .metadata()
        .map_err(|e| NativeJobError::new("invalid-source", e.to_string()))?;
    if after.len() != source_metadata.len()
        || after.modified().ok() != source_metadata.modified().ok()
    {
        return Err(NativeJobError::new(
            "invalid-source",
            "CharX source changed during import",
        ));
    }
    Ok(PreparedContent {
        format,
        metadata,
        assets,
        cas_session_id: String::new(),
        portrait_logical_id,
        module,
        owner_head: None,
    })
}

fn prepare_risum_content(
    source: &mut OpenedJobSource,
    owned_directory: &Path,
    cas: &PayloadCas,
    cas_session: &mut DurableCasJob,
    job: &JobControl,
) -> Result<PreparedContent, NativeJobError> {
    source
        .file
        .seek(SeekFrom::Start(0))
        .map_err(|error| NativeJobError::new("invalid-source", error.to_string()))?;
    let staging = JobStaging::open(owned_directory).map_err(native_format_error)?;
    let parsed = parse_risum(&mut source.file, &staging, &risum_import_limits(), &|| {
        job.is_cancel_requested()
    })
    .map_err(native_format_error)?;
    let mut module = parsed
        .metadata
        .get("module")
        .and_then(Value::as_object)
        .cloned()
        .ok_or_else(|| NativeJobError::new("invalid-input", "Risu module metadata is invalid"))?;
    let assets_present = module.contains_key("assets");
    if let Some(metadata_assets) = module.get_mut("assets").and_then(Value::as_array_mut) {
        for (position, tuple) in metadata_assets.iter_mut().enumerate() {
            let tuple = tuple.as_array_mut().ok_or_else(|| {
                NativeJobError::new(
                    "invalid-input",
                    format!("Risu module asset {position} must be a tuple"),
                )
            })?;
            if tuple.len() < 3 {
                return Err(NativeJobError::new(
                    "invalid-input",
                    format!("Risu module asset {position} tuple is incomplete"),
                ));
            }
            tuple[1] = Value::String(String::new());
            if tuple[2].is_null() {
                tuple[2] = Value::String(String::new());
            }
        }
    }
    // An asset without a declared extension is stored under its image format,
    // so it is still served as an image.
    let extensions = parsed
        .assets
        .iter()
        .map(|asset| {
            if asset.declared_extension.is_empty() {
                sniffed_image_extension(&owned_directory.join(&asset.payload.staged_name))
            } else {
                Ok(asset.declared_extension.clone())
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut assets = Vec::with_capacity(parsed.assets.len());
    let mut manifest_entries = Vec::with_capacity(parsed.assets.len());
    report_content_progress(
        job,
        JobStage::PreparingAttachments,
        0,
        Some(parsed.assets.len() as u64),
    )?;
    let inputs = parsed.assets.iter().map(|asset| ImportPayload {
        path: owned_directory.join(&asset.payload.staged_name),
        hash: asset.payload.sha256.clone(),
        size: asset.payload.byte_size,
        sync_required: false,
        progress_items: 1,
    }).collect::<Vec<_>>();
    let promoted = prepare_content_payloads(cas, cas_session, &inputs, ImportPinning::DuringPreparation, job)?;
    for ((asset, prepared), extension) in parsed.assets.into_iter().zip(promoted).zip(extensions) {
        if job.is_cancel_requested() { return Err(content_cancelled()); }
        if prepared.content_hash != asset.payload.sha256
            || prepared.byte_size != asset.payload.byte_size
        {
            return Err(NativeJobError::new(
                "invalid-source",
                "staged Risu module asset changed before CAS preparation",
            ));
        }
        let logical_id = format!(
            "assets/{}.{}",
            prepared.content_hash,
            storage_suffix(Some(&extension))
        );
        let tuple = module
            .get("assets")
            .and_then(Value::as_array)
            .and_then(|values| values.get(asset.position))
            .and_then(Value::as_array)
            .ok_or_else(|| {
                NativeJobError::new("invalid-input", "Risu module asset tuple is invalid")
            })?;
        manifest_entries.push(
            crate::asset_repository::owner_manifest_codec::OwnerManifestEntry {
                tuple: [
                    tuple[0]
                        .as_str()
                        .ok_or_else(|| {
                            NativeJobError::new(
                                "invalid-input",
                                "Risu module asset name must be a string",
                            )
                        })?
                        .to_owned(),
                    logical_id.clone(),
                    tuple[2]
                        .as_str()
                        .ok_or_else(|| {
                            NativeJobError::new(
                                "invalid-input",
                                "Risu module asset extension must be a string",
                            )
                        })?
                        .to_owned(),
                ],
                payload_hash: Some(
                    hex::decode(&prepared.content_hash)
                        .expect("prepared payload hash is hexadecimal")
                        .try_into()
                        .expect("prepared payload hash is SHA-256"),
                ),
            },
        );
        assets.push(PreparedContentAsset {
            reference_key: String::new(),
            token: String::new(),
            position: Some(asset.position),
            logical_id,
            object_hash: prepared.content_hash,
            byte_size: prepared.byte_size,
            mime: String::new(),
            name: String::new(),
            ext: extension,
        });
    }
    let owner_head = if assets_present {
        let bytes =
            crate::asset_repository::owner_manifest_codec::encode_owner_manifest(&manifest_entries)
                .map_err(|error| NativeJobError::new("invalid-input", error.to_string()))?;
        let prepared = cas_session
            .prepare_bytes(cas, &bytes, CasObjectRole::OwnerManifest)
            .map_err(|error| NativeJobError::new("store-error", error.to_string()))?;
        let expected_hash = hex::encode(Sha256::digest(&bytes));
        if prepared.content_hash != expected_hash {
            return Err(NativeJobError::new(
                "store-error",
                "prepared Risu module owner manifest identity mismatch",
            ));
        }
        PreparedContentOwnerHead {
            present: true,
            manifest_hash: Some(prepared.content_hash),
            entry_count: manifest_entries.len(),
        }
    } else {
        PreparedContentOwnerHead {
            present: false,
            manifest_hash: None,
            entry_count: 0,
        }
    };
    Ok(PreparedContent {
        format: PreparedContentFormat::RisuModule,
        metadata: Value::Object(module),
        assets,
        cas_session_id: String::new(),
        portrait_logical_id: None,
        module: None,
        owner_head: Some(owner_head),
    })
}

fn promote_archive_asset_occurrences(
    descriptor: &ParsedCharXDescriptor,
    cas: &PayloadCas,
    cas_session: &mut DurableCasJob,
    job: &JobControl,
) -> Result<Vec<PreparedContentAsset>, NativeJobError> {
    let payloads = descriptor
        .payloads
        .iter()
        .map(|payload| (payload.normalized_name.as_str(), payload))
        .collect::<HashMap<_, _>>();
    let mut names = HashMap::<&str, usize>::new();
    let mut unique = Vec::new();
    for reference in &descriptor.asset_references {
        let count = names.entry(reference.normalized_name.as_str()).or_default();
        *count += 1;
        if *count == 1 {
            let payload = payloads.get(reference.normalized_name.as_str()).ok_or_else(|| {
                NativeJobError::new("invalid-input", "CharX card reference has no staged payload")
            })?;
            unique.push(*payload);
        }
    }
    let inputs = unique.iter().map(|payload| ImportPayload {
        path: payload.staged_path.clone(), hash: payload.sha256.clone(), size: payload.decoded_size,
        sync_required: false, progress_items: names[payload.normalized_name.as_str()],
    }).collect::<Vec<_>>();
    let prepared = prepare_content_payloads(cas, cas_session, &inputs, ImportPinning::AfterContentMapping, job)?;
    let promoted = unique.iter().zip(prepared).map(|(payload, result)|
        (payload.normalized_name.as_str(), result)).collect::<HashMap<_, _>>();
    let mut assets = Vec::with_capacity(descriptor.asset_references.len());
    for reference in &descriptor.asset_references {
        if job.is_cancel_requested() {
            return Err(content_cancelled());
        }
        let payload = payloads
            .get(reference.normalized_name.as_str())
            .ok_or_else(|| {
                NativeJobError::new(
                    "invalid-input",
                    "CharX card reference has no staged payload",
                )
            })?;
        let promoted_payload = &promoted[reference.normalized_name.as_str()];
        let token = format!("native-charx-{}", reference.order);
        let suffix = storage_suffix(payload.normalized_extension.as_deref());
        let ext = reference
            .declared_extension
            .clone()
            .unwrap_or_else(|| suffix.clone());
        assets.push(PreparedContentAsset {
            reference_key: token.clone(),
            token,
            position: None,
            logical_id: format!("assets/{}.{}", promoted_payload.content_hash, suffix),
            object_hash: promoted_payload.content_hash.clone(),
            byte_size: promoted_payload.byte_size,
            mime: payload.mime_type.clone(),
            name: reference.display_name.clone().unwrap_or_default(),
            ext,
        });
    }
    Ok(assets)
}

fn rewrite_archive_asset_occurrences(
    descriptor: &ParsedCharXDescriptor,
    metadata: &mut Value,
) -> Result<(), NativeJobError> {
    let payloads = descriptor
        .payloads
        .iter()
        .map(|payload| payload.normalized_name.as_str())
        .collect::<HashSet<_>>();
    for reference in &descriptor.asset_references {
        if !payloads.contains(reference.normalized_name.as_str()) {
            return Err(NativeJobError::new(
                "invalid-input",
                "CharX card reference has no staged payload",
            ));
        }
        let uri_pointer = format!("/data/assets/{}/uri", reference.order);
        let uri = metadata.pointer_mut(&uri_pointer).ok_or_else(|| {
            NativeJobError::new("invalid-input", "CharX card reference cannot be rewritten")
        })?;
        *uri = Value::String(format!("__asset:native-charx-{}", reference.order));
    }
    Ok(())
}

fn promote_json_assets(
    metadata: &Value,
    payloads: Vec<JsonCardPayload>,
    staging_root: &Path,
    cas: &PayloadCas,
    cas_session: &mut DurableCasJob,
    job: &JobControl,
) -> Result<Vec<PreparedContentAsset>, NativeJobError> {
    let mut assets = Vec::with_capacity(payloads.len());
    let inputs = payloads.iter().map(|item| ImportPayload {
        path: staging_root.join(&item.payload.staged_name),
        hash: item.payload.sha256.clone(), size: item.payload.byte_size, sync_required: false, progress_items: 1,
    }).collect::<Vec<_>>();
    let promoted = prepare_content_payloads(cas, cas_session, &inputs, ImportPinning::AfterContentMapping, job)?;
    for (payload, promoted) in payloads.into_iter().zip(promoted) {
        if job.is_cancel_requested() {
            return Err(content_cancelled());
        }
        let JsonCardPayload {
            json_pointer,
            reference_key,
            media_type,
            extension,
            ..
        } = payload;
        let name_pointer = json_pointer
            .strip_suffix("/uri")
            .map(|pointer| format!("{pointer}/name"));
        let name = name_pointer
            .as_deref()
            .and_then(|pointer| metadata.pointer(pointer))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let suffix = storage_suffix(Some(&extension));
        assets.push(PreparedContentAsset {
            token: reference_key.clone(),
            reference_key,
            position: None,
            logical_id: format!("assets/{}.{}", promoted.content_hash, suffix),
            object_hash: promoted.content_hash,
            byte_size: promoted.byte_size,
            mime: media_type,
            name,
            ext: extension,
        });
    }
    Ok(assets)
}

fn prepare_content_payloads(
    cas: &PayloadCas,
    cas_session: &mut DurableCasJob,
    inputs: &[ImportPayload],
    pinning: ImportPinning,
    job: &JobControl,
) -> Result<Vec<PreparedPayload>, NativeJobError> {
    let mut reported = 0;
    let mut progress_error = None;
    let prepared = prepare_import_payloads(cas, cas_session, inputs, pinning, &|| job.is_cancel_requested(), |_, count, _| {
        if reported < count {
            if let Err(error) = advance_content_assets(job, (count - reported) as u64) {
                progress_error.get_or_insert(error);
            }
            reported = count;
        }
    }).map_err(|error| {
        if job.is_cancel_requested() { content_cancelled() }
        else { NativeJobError::new("store-error", error.to_string()) }
    })?;
    if let Some(error) = progress_error { return Err(error); }
    Ok(prepared)
}

fn parse_root_module_overlay(
    descriptor: &ParsedCharXDescriptor,
    owned_directory: &Path,
    job: &JobControl,
) -> Result<Option<PreparedContentModule>, NativeJobError> {
    let Some(module_payload) = descriptor
        .payloads
        .iter()
        .find(|payload| payload.normalized_name == "module.risum")
    else {
        return Ok(None);
    };
    let staging = JobStaging::open(owned_directory).map_err(native_format_error)?;
    let mut source = open_regular_file_no_follow(&module_payload.staged_path)?.file;
    let parsed = parse_risum(&mut source, &staging, &content_import_limits(), &|| {
        job.is_cancel_requested()
    })
    .map_err(native_format_error)?;
    let module = parsed
        .metadata
        .get("module")
        .and_then(Value::as_object)
        .ok_or_else(|| NativeJobError::new("invalid-input", "Risu module metadata is invalid"))?;
    Ok(Some(PreparedContentModule {
        trigger: bounded_module_array(module, "trigger")?.unwrap_or_default(),
        regex: bounded_module_array(module, "regex")?.unwrap_or_default(),
        lorebook: bounded_module_array(module, "lorebook")?,
    }))
}

fn bounded_module_array(
    module: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<Option<Vec<Value>>, NativeJobError> {
    let Some(value) = module.get(field) else {
        return Ok(None);
    };
    let values = value.as_array().ok_or_else(|| {
        NativeJobError::new(
            "invalid-input",
            format!("Risu module {field} must be an array"),
        )
    })?;
    if values.len() > MAX_MODULE_OVERLAY_ITEMS {
        return Err(NativeJobError::too_large("module", format!("Risu module {field} exceeds its item limit"),
        ));
    }
    Ok(Some(values.clone()))
}

fn promote_jpeg_prefix(
    source: &mut OpenedJobSource,
    archive_offset: u64,
    cas: &PayloadCas,
    job: &JobControl,
) -> Result<PreparedContentAsset, NativeJobError> {
    if archive_offset == 0 {
        return Err(NativeJobError::new(
            "invalid-input",
            "appended CharX JPEG has no portrait prefix",
        ));
    }
    if job.is_cancel_requested() {
        return Err(content_cancelled());
    }
    source
        .file
        .seek(SeekFrom::Start(0))
        .map_err(|e| NativeJobError::new("invalid-source", e.to_string()))?;
    let promoted = prepare_cancellable_payload(cas, (&mut source.file).take(archive_offset), job)?;
    if promoted.byte_size != archive_offset {
        return Err(NativeJobError::new(
            "invalid-source",
            "appended CharX JPEG portrait prefix is truncated",
        ));
    }
    if job.is_cancel_requested() {
        return Err(content_cancelled());
    }
    let token = "native-appended-portrait".to_owned();
    let object_hash = promoted.content_hash;
    Ok(PreparedContentAsset {
        reference_key: token.clone(),
        token,
        position: None,
        logical_id: format!("assets/{object_hash}.jpg"),
        object_hash,
        byte_size: promoted.byte_size,
        mime: "image/jpeg".to_owned(),
        name: String::new(),
        ext: "jpg".to_owned(),
    })
}

fn open_payload_cas(repository_root: &Path) -> Result<PayloadCas, NativeJobError> {
    PayloadCas::new(repository_root)
        .map_err(|error| NativeJobError::new("store-error", error.to_string()))
}

fn prepare_cancellable_payload<R: Read>(
    cas: &PayloadCas,
    reader: R,
    job: &JobControl,
) -> Result<PreparedPayload, NativeJobError> {
    let mut reader = CancellableReader { inner: reader, job };
    cas.prepare_reader(&mut reader).map_err(|error| {
        if job.is_cancel_requested() {
            content_cancelled()
        } else {
            NativeJobError::new("store-error", error.to_string())
        }
    })
}

fn begin_content_cas_session(
    repository_root: &Path,
    job: &JobControl,
) -> Result<DurableCasJob, NativeJobError> {
    let created_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| NativeJobError::new("store-error", error.to_string()))?
        .as_millis();
    let created_at_ms = i64::try_from(created_at_ms).map_err(|_| {
        NativeJobError::new(
            "store-error",
            "system time exceeds the supported millisecond range",
        )
    })?;
    DurableCasJob::begin(
        repository_root,
        &job.id(),
        CasJobKind::CardOrModuleContentImport,
        crate::asset_repository::job_pins::CasJobOwner::content_import(&job.id()),
        created_at_ms,
    )
    .map_err(|error| NativeJobError::new("store-error", error.to_string()))
}

fn abort_failed_content_session(
    mut cas_session: DurableCasJob,
    error: NativeJobError,
) -> NativeJobError {
    match cas_session.release(CasReleaseOutcome::Aborted) {
        Ok(()) => error,
        Err(abort_error) => NativeJobError::new(
            "cleanup-failed",
            format!("{}; CAS session abort failed: {abort_error}", error.message),
        ),
    }
}

fn content_import_limits() -> ImportLimits {
    ImportLimits {
        max_metadata_bytes: JSON_CARD_MAX_METADATA_BYTES,
        max_payload_bytes: 64 * 1024 * 1024,
        max_aggregate_payload_bytes: 10 * 1024 * 1024 * 1024,
        max_payload_count: 50_000,
        max_container_entries: 4096,
        max_container_directory_bytes: 32 * 1024 * 1024,
        charx_probe_metadata_bytes: JSON_CARD_MAX_METADATA_BYTES as u64,
    }
}

pub(super) fn risum_import_limits() -> ImportLimits {
    content_import_limits()
}

pub(super) fn content_classification_limits() -> ImportLimits {
    let charx_limits = CharXLimits::default();
    let mut limits = content_import_limits();
    limits.max_container_entries = charx_limits.max_entries;
    limits.max_container_directory_bytes = charx_limits.max_directory_bytes;
    limits.charx_probe_metadata_bytes = charx_limits.max_metadata_bytes;
    limits
}

fn sniffed_image_extension(path: &Path) -> Result<String, NativeJobError> {
    let mut head = Vec::with_capacity(16);
    open_regular_file_no_follow(path)?
        .file
        .take(16)
        .read_to_end(&mut head)
        .map_err(|error| NativeJobError::new("invalid-source", error.to_string()))?;
    Ok(image::guess_format(&head)
        .ok()
        .and_then(|format| format.extensions_str().first())
        .map(|extension| (*extension).to_owned())
        .unwrap_or_default())
}

fn storage_suffix(extension: Option<&str>) -> String {
    let extension = extension.unwrap_or("bin");
    if !extension.is_empty()
        && extension.len() <= 32
        && extension
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'_' | b'-'))
    {
        extension.to_ascii_lowercase()
    } else {
        "bin".to_owned()
    }
}

pub(super) fn native_format_error(error: FormatError) -> NativeJobError {
    if matches!(error.kind, FormatErrorKind::LimitExceeded) {
        return NativeJobError::too_large("content", error.message);
    }
    let code = match error.kind {
        FormatErrorKind::Cancelled => "cancelled",
        FormatErrorKind::InvalidFormat | FormatErrorKind::LimitExceeded => "invalid-input",
        FormatErrorKind::Io => "invalid-source",
    };
    NativeJobError::new(code, error.message)
}

fn native_charx_error(error: CharXParseError) -> NativeJobError {
    if matches!(error.code(), CharXParseErrorCode::TooManyEntries | CharXParseErrorCode::EntryTooLarge
        | CharXParseErrorCode::AggregateTooLarge | CharXParseErrorCode::MetadataTooLarge
        | CharXParseErrorCode::CompressionRatioExceeded) {
        return NativeJobError::too_large("card", error.to_string());
    }
    let code = match error.code() {
        CharXParseErrorCode::Cancelled => "cancelled",
        CharXParseErrorCode::Io => "invalid-source",
        _ => "invalid-input",
    };
    NativeJobError::new(code, error.to_string())
}

fn native_png_error(error: PngCardError) -> NativeJobError {
    if matches!(error, PngCardError::LimitExceeded(_)) {
        return NativeJobError::too_large("card", error.to_string());
    }
    let code = match error {
        PngCardError::Cancelled => "cancelled",
        PngCardError::Io(_) => "invalid-source",
        PngCardError::Invalid(_) => "invalid-input",
        PngCardError::LimitExceeded(_) => "native-limit",
    };
    NativeJobError::new(code, error.to_string())
}

fn report_content_progress(
    job: &JobControl,
    stage: JobStage,
    completed: u64,
    total: Option<u64>,
) -> Result<(), NativeJobError> {
    job.set_detail(JobDetail::new(
        stage,
        StageUnit::Items,
        completed,
        total,
        ImportCounts {
            assets: completed,
            attachments_prepared: completed,
            ..Default::default()
        },
    ))
    .map_err(|error| {
        if job.is_cancel_requested() {
            content_cancelled()
        } else {
            NativeJobError::new("store-error", error)
        }
    })
}

fn advance_content_assets(job: &JobControl, count: u64) -> Result<(), NativeJobError> {
    let detail = job.status().detail;
    let completed = detail.as_ref().map_or(0, |d| d.counts.attachments_prepared) + count;
    let total = detail.and_then(|d| {
        if d.stage == JobStage::PreparingAttachments {
            d.stage_total
        } else {
            None
        }
    });
    report_content_progress(job, JobStage::PreparingAttachments, completed, total)
}

#[cfg(test)]
mod large_metadata_tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine as _};

    #[test]
    fn json_cards_accept_a_thousand_inline_assets_without_the_old_256_item_limit() {
        let directory = tempfile::tempdir().unwrap();
        let staging = JobStaging::open(directory.path()).unwrap();
        let assets: Vec<Value> = (0_u32..1000).map(|index| serde_json::json!({
            "type": "x-risu-asset", "name": index.to_string(), "ext": "bin",
            "uri": format!("data:application/octet-stream;base64,{}", STANDARD.encode(index.to_le_bytes()))
        })).collect();
        let document = serde_json::to_vec(&serde_json::json!({
            "spec": "chara_card_v3", "spec_version": "3.0",
            "data": { "name": "synthetic", "assets": assets }
        }))
        .unwrap();
        let parsed = parse_json_card(
            &mut document.as_slice(),
            &staging,
            &content_import_limits(),
            &|| false,
        )
        .unwrap();
        assert_eq!(parsed.payloads.len(), 1000);
        for (index, payload) in parsed.payloads.iter().enumerate() {
            assert_eq!(
                payload.payload.sha256,
                hex::encode(Sha256::digest((index as u32).to_le_bytes()))
            );
        }
    }
}


pub(super) struct InlineAssetUpload {
    file: Option<std::fs::File>,
    directory: PathBuf,
    jobs_root: PathBuf,
    job_id: String,
    name: String,
    total_bytes: u64,
    received: u64,
}

impl Drop for InlineAssetUpload {
    fn drop(&mut self) {
        drop(self.file.take());
        let _ = super::cleanup_one_owned_directory(&self.jobs_root, &self.directory, &self.job_id);
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InlineAssetStageResult {
    received: u64,
    asset: Option<PreparedContentAsset>,
}

impl super::NativeFileJobState {
    pub(super) fn stage_inline_asset(
        &self,
        job_id: &str,
        session: &DurableCasJob,
        name: &str,
        total_bytes: u64,
        offset: u64,
        data: &[u8],
    ) -> Result<InlineAssetStageResult, NativeJobError> {
        let _cleanup = self.admit_cleanup_operation()?;
        let invalid = |message: &str| NativeJobError::new("invalid-input", message);
        let store_error = |error: std::io::Error| NativeJobError::new("store-error", error.to_string());
        let limits = PngCardLimits::default();
        if session.kind() != CasJobKind::CardOrModuleContentImport || session.is_sealed()
            || session.is_released() || session.pin_count() != 0
        {
            return Err(invalid("Native content is not awaiting asset mapping"));
        }
        let ext = if name.is_empty() { "png" } else { name.rsplit('.').next().unwrap_or("png") };
        if ext.is_empty() || ext.len() > 32 || !ext.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"+_-".contains(&byte)) {
            return Err(invalid("Inline asset extension is invalid"));
        }
        if total_bytes > limits.max_embedded_asset_bytes {
            return Err(NativeJobError::too_large("card", "PNG embedded asset exceeds its limit"));
        }
        if data.len() > COPY_BUFFER_BYTES
            || offset > total_bytes || data.len() as u64 > total_bytes - offset
            || (data.is_empty() && total_bytes != 0) || name.len() > 4096
        {
            return Err(invalid("Inline asset chunk or size is invalid"));
        }
        let job = self.registry.lookup(job_id).map_err(|error| invalid(&error))?
            .ok_or_else(|| invalid("Native content job is missing"))?;
        let mut status = job.status.lock().map_err(|error| invalid(&error.to_string()))?;
        if status.kind != super::JobKind::PrepareContentImport || status.state != super::JobState::Succeeded
            || job.is_cancel_requested()
        {
            return Err(invalid("Native content is not awaiting asset mapping"));
        }
        let prepared = status.prepared_content.as_mut().ok_or_else(|| invalid("Native content receipt is missing"))?;
        if prepared.format != PreparedContentFormat::PngCard || prepared.cas_session_id != job_id {
            return Err(invalid("Inline asset staging requires prepared PNG content"));
        }
        let embedded = prepared.assets.iter().skip(usize::from(prepared.portrait_logical_id.is_some()));
        let mut count = 0_usize;
        let mut total = 0_u64;
        for asset in embedded {
            count += 1;
            total = total.checked_add(asset.byte_size).ok_or_else(|| invalid("PNG embedded asset size overflow"))?;
        }
        if count >= limits.max_embedded_asset_count || total.checked_add(total_bytes)
            .is_none_or(|total| total > limits.max_embedded_asset_total_bytes)
        {
            return Err(NativeJobError::too_large("card", "PNG embedded asset count or total exceeds its limit"));
        }
        let mut wait = job.wait_state.lock().map_err(|error| invalid(&error.to_string()))?;
        let mut upload = match wait.inline_asset_upload.take() {
            Some(upload) => upload,
            None if offset == 0 => {
                let jobs_root = self.root.join("jobs");
                let directory = super::create_owned_directory(&jobs_root, job_id).map_err(|error| invalid(&error))?;
                let mut upload = InlineAssetUpload { file: None, directory, jobs_root,
                    job_id: job_id.to_owned(), name: name.to_owned(), total_bytes, received: 0 };
                upload.file = Some(std::fs::OpenOptions::new().write(true).read(true).create_new(true)
                    .open(upload.directory.join("inline-asset" )).map_err(store_error)?);
                upload
            }
            None => return Err(invalid("Inline asset upload is missing")),
        };
        if upload.received != offset || upload.total_bytes != total_bytes || upload.name != name {
            return Err(invalid("Inline asset upload does not match its chunk"));
        }
        let file = upload.file.as_mut().expect("inline upload owns its file");
        file.write_all(data).map_err(store_error)?;
        upload.received += data.len() as u64;
        if upload.received != total_bytes {
            let received = upload.received;
            wait.inline_asset_upload = Some(upload);
            return Ok(InlineAssetStageResult { received, asset: None });
        }
        file.sync_all().map_err(store_error)?;
        file.seek(SeekFrom::Start(0)).map_err(store_error)?;
        let repository_root = self.root.parent().ok_or_else(|| invalid("Native job repository is missing"))?;
        let payload = PayloadCas::new(repository_root).map_err(store_error)?.prepare_reader(file).map_err(store_error)?;
        let created_at_ms = SystemTime::now().duration_since(UNIX_EPOCH)
            .map_err(|error| invalid(&error.to_string()))?.as_millis().min(i64::MAX as u128) as i64;
        let _guard = crate::asset_repository::coordinator::lock_repository_mutation().map_err(store_error)?;
        crate::persistent_store::register_asset_objects_at_root(repository_root,
            &[crate::persistent_store::asset_object_catalog::AssetObjectRegistration {
                object_hash: payload.content_hash.clone(), byte_size: payload.byte_size,
            }], created_at_ms).map_err(|error| invalid(&error.to_string()))?;
        let logical_id = format!("assets/{}.{}", payload.content_hash, ext);
        let token = loop {
            let token = format!("native-inline-{}", uuid::Uuid::new_v4());
            if prepared.assets.iter().all(|asset| asset.token != token) { break token; }
        };
        let asset = PreparedContentAsset {
            reference_key: token.clone(), token, position: None, logical_id,
            object_hash: payload.content_hash.clone(), byte_size: payload.byte_size, mime: String::new(),
            name: if name.is_empty() { format!("{}.{}", payload.content_hash, ext) } else { name.to_owned() },
            ext: ext.to_owned(),
        };
        prepared.assets.push(asset.clone());
        Ok(InlineAssetStageResult { received: total_bytes, asset: Some(asset) })
    }
}


#[cfg(test)]
mod inline_asset_tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, super::super::NativeFileJobState, String, DurableCasJob) {
        let root = tempfile::tempdir().unwrap();
        crate::persistent_store::PersistentStore::open(root.path()).unwrap();
        let state = super::super::NativeFileJobState::initialize(root.path().join("native-file-jobs"));
        let job = state.registry.create(super::super::JobKind::PrepareContentImport).unwrap();
        let id = job.id();
        let session = DurableCasJob::begin(root.path(), &id, CasJobKind::CardOrModuleContentImport, crate::asset_repository::job_pins::CasJobOwner::for_test(), 1).unwrap();
        job.start(JobPhase::ReadingSource).unwrap();
        job.finish_content_job(Ok(PreparedContent {
            format: PreparedContentFormat::PngCard, metadata: serde_json::json!({"chara":"synthetic"}),
            assets: Vec::new(), cas_session_id: id.clone(), portrait_logical_id: None, module: None, owner_head: None,
        }), Ok(())).unwrap();
        (root, state, id, session)
    }

    #[test]
    fn inline_asset_chunks_are_owned_bounded_and_part_of_the_native_receipt() {
        let (root, state, id, session) = fixture();
        let bytes = vec![41_u8; COPY_BUFFER_BYTES + 3];
        let first = state.stage_inline_asset(&id, &session, "sample.wav", bytes.len() as u64, 0, &bytes[..COPY_BUFFER_BYTES]).unwrap();
        assert_eq!(first.received, COPY_BUFFER_BYTES as u64);
        assert!(first.asset.is_none());
        assert!(state.root.join("jobs").join(&id).exists());
        let final_chunk = state.stage_inline_asset(&id, &session, "sample.wav", bytes.len() as u64,
            COPY_BUFFER_BYTES as u64, &bytes[COPY_BUFFER_BYTES..]).unwrap();
        let asset = final_chunk.asset.unwrap();
        assert_eq!(asset.ext, "wav");
        assert_eq!(asset.byte_size, bytes.len() as u64);
        assert_eq!(PayloadCas::new(root.path()).unwrap().read_object(&asset.object_hash).unwrap(), Some(bytes));
        assert_eq!(state.content_asset_receipt(&id).unwrap(), [(asset.object_hash.clone(), asset.byte_size)]);
        assert!(!state.root.join("jobs").join(&id).exists());
        let store = crate::persistent_store::PersistentStore::open(root.path()).unwrap();
        assert_eq!(store.query_asset_object_catalog(1, None).unwrap().items[0].object_hash, asset.object_hash);
    }

    #[test]
    fn inline_asset_rejects_sealed_released_and_forgotten_jobs() {
        let (root, state, id, mut session) = fixture();
        let mut store = crate::persistent_store::PersistentStore::open(root.path()).unwrap();
        session.seal(&mut store, 1).unwrap();
        assert!(state.stage_inline_asset(&id, &session, "", 1, 0, &[1]).is_err());
        session.release(CasReleaseOutcome::Aborted).unwrap();
        assert!(state.stage_inline_asset(&id, &session, "", 1, 0, &[1]).is_err());
        state.forget(&id).unwrap();
        assert!(state.stage_inline_asset(&id, &session, "", 1, 0, &[1]).is_err());
    }

    #[test]
    fn inline_asset_limits_and_cancel_cleanup_keep_no_staging_files() {
        let (_root, state, id, session) = fixture();
        let limits = PngCardLimits::default();
        assert!(state.stage_inline_asset(&id, &session, "", limits.max_embedded_asset_bytes + 1, 0, &[1]).is_err());
        assert!(state.stage_inline_asset(&id, &session, "", (COPY_BUFFER_BYTES + 1) as u64, 0, &vec![1; COPY_BUFFER_BYTES + 1]).is_err());
        state.stage_inline_asset(&id, &session, "", 2, 0, &[1]).unwrap();
        assert!(state.root.join("jobs").join(&id).exists());
        state.forget(&id).unwrap();
        assert!(!state.root.join("jobs").join(&id).exists());
    }

    #[test]
    fn inline_asset_counts_existing_embedded_assets_against_all_limits() {
        for count_limit in [false, true] {
            let (_root, state, id, session) = fixture();
            let job = state.registry.lookup(&id).unwrap().unwrap();
            let limits = PngCardLimits::default();
            let fake = PreparedContentAsset { reference_key: "synthetic".into(), token: "synthetic".into(),
                position: None, logical_id: "assets/synthetic.png".into(), object_hash: "a".repeat(64),
                byte_size: if count_limit { 0 } else { limits.max_embedded_asset_total_bytes },
                mime: String::new(), name: "synthetic.png".into(), ext: "png".into() };
            job.status.lock().unwrap().prepared_content.as_mut().unwrap().assets =
                vec![fake; if count_limit { limits.max_embedded_asset_count } else { 1 }];
            assert!(state.stage_inline_asset(&id, &session, "", 1, 0, &[1]).is_err());
            assert!(!state.root.join("jobs").join(&id).exists());
        }
    }
}
