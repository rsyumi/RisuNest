//! Target attachment materialization. Only verified, job-owned files reach the writer.
use super::*;
use crate::server_sync::residency::RemotePayloadAccess;
use sha2::{Digest, Sha256};
use std::path::Path;

#[derive(Default)]
pub(super) struct PreparedAttachments {
    pub(super) entries: Vec<LegacyBackupWriteEntry>,
    pub(super) owner_replacement_keys: HashMap<AssetOwnerLocator, Vec<String>>,
    pub(super) replacements: HashMap<String, String>,
    pub(super) inlay_replacements: HashMap<String, String>,
    pub(super) warning_codes: Vec<String>,
    /// Code, affected item count, source bytes. No record content is exposed.
    pub(super) losses: Vec<(String, u64, u64)>,
    pub(super) preserved_files: u64,
    pub(super) preserved_bytes: u64,
    pub(super) affected_conversations: HashMap<String, Option<u64>>,
    owner_playback_unverified: HashSet<AssetOwnerLocator>,
}

fn invalid_source(message: &'static str) -> NativeJobError {
    NativeJobError::new("invalid-source", message)
}

fn safe_component(value: &str) -> bool {
    if value.is_empty()
        || value.encode_utf16().count() > 255
        || value.ends_with(['.', ' '])
        || value.chars().any(|c| {
            c.is_control() || matches!(c, '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*')
        })
    {
        return false;
    }
    let stem = value
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end_matches(' ')
        .to_uppercase();
    !matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) && !["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    })
}

fn name_identity(value: &str) -> String {
    // Reserve case-insensitively even when the export is prepared on Unix.
    value.to_uppercase()
}

fn safe_inlay_id(value: &str) -> bool {
    // Pocket's provenance namespace rejects every `/..` occurrence.
    safe_component(value)
        && safe_component(&format!("{value}.meta.json"))
        && !value.starts_with("..")
}

fn reserve_inlay_files(id: &str, ext: &str, used: &mut HashSet<String>) -> bool {
    let payload = format!("{id}.{ext}");
    let sidecar = format!("{id}.meta.json");
    if !safe_component(&payload) || !safe_component(&sidecar) {
        return false;
    }
    let payload = name_identity(&payload);
    let sidecar = name_identity(&sidecar);
    if payload == sidecar || used.contains(&payload) || used.contains(&sidecar) {
        return false;
    }
    used.insert(payload);
    used.insert(sidecar);
    true
}

fn stable_id(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

fn normalized_extension(value: &str) -> String {
    let value = value.trim().to_ascii_lowercase();
    let value: String = value
        .trim_start_matches('.')
        .chars()
        .filter(|c| !matches!(c, '/' | '\\' | '\0'))
        .collect();
    if value.is_empty() {
        "bin".to_owned()
    } else {
        value
    }
}

fn inlay_extension(value: &str) -> String {
    let normalized = normalized_extension(value);
    // Its raw attachment parser splits on the last dot. A multi-part extension
    // would become part of the imported ID and disconnect the sidecar.
    if normalized.contains('.') || !safe_component(&format!("{}.{normalized}", "a".repeat(64))) {
        "bin".to_owned()
    } else {
        normalized
    }
}

fn supported_image_extension(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "png" | "jpeg" | "jpg" | "gif" | "webp" | "avif"
    )
}

fn supported_image_mime(value: &str) -> bool {
    matches!(
        value
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "image/png" | "image/jpeg" | "image/gif" | "image/webp" | "image/avif"
    )
}

fn warning(result: &mut PreparedAttachments, code: &str) {
    if !result.warning_codes.iter().any(|value| value == code) {
        result.warning_codes.push(code.to_owned());
    }
}

fn loss(result: &mut PreparedAttachments, code: &str, bytes: u64) {
    if let Some(item) = result.losses.iter_mut().find(|item| item.0 == code) {
        item.1 += 1;
        item.2 = item.2.saturating_add(bytes);
    } else {
        result.losses.push((code.to_owned(), 1, bytes));
    }
    warning(result, code);
}

/// Prefer an existing flat name. Reserve all valid source names before allocation,
/// so generated names cannot steal the name of a later logical source.
fn asset_name(key: &str, target: CompatibilityTarget) -> Option<String> {
    let name = key.strip_prefix("assets/")?;
    let lower = name.to_ascii_lowercase();
    if !safe_component(name)
        || matches!(lower.as_str(), DATABASE_ENTRY | ENCRYPTION_ENTRY)
        || lower.starts_with("coldstorage_")
        || lower.starts_with("inlay_")
    {
        return None;
    }
    if target == CompatibilityTarget::RisuAi && !name.ends_with(".png") {
        return None;
    }
    Some(name.to_owned())
}

fn allocate_asset_name(key: &str, extension: &str, used: &mut HashSet<String>) -> String {
    let mut index = 0u64;
    loop {
        let name = format!("asset-{}.{extension}", stable_id(&format!("{key}:{index}")));
        if used.insert(name_identity(&name)) {
            return name;
        }
        index += 1;
    }
}

/// Pins then streams into a private file, checking the exact hash and length.
fn copy_verified(
    cas: &PayloadCas,
    pins: &mut DurableCasJob,
    hash: &str,
    expected: u64,
    role: CasObjectRole,
    destination: &Path,
    cancellation: &dyn CancellationProbe,
) -> Result<(), NativeJobError> {
    if expected > u64::from(u32::MAX) {
        return Err(NativeJobError::new(
            "length-overflow",
            "attachment exceeds the target's 4 GiB entry limit",
        ));
    }
    check_cancelled(cancellation).map_err(local_backup_error)?;
    pins.pin_existing(cas, hash, expected, role)
        .map_err(io_job_error)?;
    let mut source = cas
        .open_available_object(hash)
        .map_err(io_job_error)?
        .ok_or_else(|| invalid_source("attachment object is missing"))?;
    let mut output = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .map_err(io_job_error)?;
    let mut digest = Sha256::new();
    let mut bytes = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        check_cancelled(cancellation).map_err(local_backup_error)?;
        let count = source.read(&mut buffer).map_err(io_job_error)?;
        if count == 0 {
            break;
        }
        bytes += count as u64;
        if bytes > expected {
            return Err(invalid_source("attachment object length changed"));
        }
        digest.update(&buffer[..count]);
        output.write_all(&buffer[..count]).map_err(io_job_error)?;
    }
    if bytes != expected || hex::encode(digest.finalize()) != hash {
        return Err(invalid_source("attachment object hash or length mismatch"));
    }
    output.sync_all().map_err(io_job_error)
}

fn write_json(path: &Path, value: &Value) -> Result<(), NativeJobError> {
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(io_job_error)?;
    serde_json::to_writer(&mut file, value)
        .map_err(|_| invalid_source("attachment metadata cannot be serialized"))?;
    file.sync_all().map_err(io_job_error)
}

fn provenance(alias: &AssetAlias) -> Option<Value> {
    let source = alias.metadata.get("pocketRisu")?.as_object()?;
    let mut result = Map::new();
    for key in ["createdAt", "updatedAt"] {
        if let Some(value) = source
            .get(key)
            .filter(|v| v.as_f64().is_some_and(|n| n >= 0.0))
        {
            result.insert(key.to_owned(), value.clone());
        }
    }
    for key in ["charId", "chatId"] {
        if let Some(value) = source.get(key).filter(|v| v.is_string()) {
            result.insert(key.to_owned(), value.clone());
        }
    }
    if result.is_empty() {
        None
    } else {
        Some(Value::Object(result))
    }
}

pub(super) fn prepare(
    target: CompatibilityTarget,
    owned_directory: &Path,
    cas: &PayloadCas,
    inventory: &export::PinnedLegacyBackupInventory,
    pins: &mut DurableCasJob,
    cancellation: &dyn CancellationProbe,
) -> Result<PreparedAttachments, NativeJobError> {
    let mut result = PreparedAttachments::default();
    let manifests: HashSet<&str> = inventory
        .owner_heads
        .iter()
        .filter(|h| h.present)
        .filter_map(|h| h.manifest_hash.as_deref())
        .collect();
    let role = |hash: &str| {
        if manifests.contains(hash) {
            CasObjectRole::OwnerManifest
        } else {
            CasObjectRole::DirectObject
        }
    };
    let mut names = HashSet::from([
        name_identity(DATABASE_ENTRY),
        name_identity(ENCRYPTION_ENTRY),
    ]);
    let mut original_asset_names = HashMap::new();
    let mut aliases: Vec<_> = inventory.assets.iter().collect();
    aliases.sort_by(|a, b| a.key.cmp(&b.key).then(a.kind.cmp(&b.kind)));
    for alias in &aliases {
        if alias.kind == "asset" {
            if let Some(name) = asset_name(&alias.key, target) {
                if names.insert(name_identity(&name)) {
                    original_asset_names.insert(alias.key.as_str(), name);
                }
            }
        }
    }
    let mut original_inlay_ids = HashSet::new();
    let mut inlay_files = HashSet::new();
    for alias in aliases.iter().filter(|a| a.kind == "inlay") {
        let ext = inlay_extension(&alias.ext);
        if safe_inlay_id(&alias.key) && reserve_inlay_files(&alias.key, &ext, &mut inlay_files) {
            original_inlay_ids.insert(alias.key.as_str());
        }
    }
    let mut seen_aliases = HashSet::new();
    for (index, alias) in aliases.iter().enumerate() {
        check_cancelled(cancellation).map_err(local_backup_error)?;
        if !seen_aliases.insert((&alias.kind, &alias.key)) {
            return Err(invalid_source("duplicate attachment alias"));
        }
        if alias.kind == "inlay" && target == CompatibilityTarget::RisuAi {
            loss(
                &mut result,
                "risuai-inlays-excluded",
                u64::try_from(alias.size).unwrap_or(0),
            );
            continue;
        }
        let hash = alias
            .object_hash
            .as_deref()
            .ok_or_else(|| invalid_source("attachment alias has no payload"))?;
        let size = u64::try_from(alias.size)
            .map_err(|_| invalid_source("attachment length is invalid"))?;
        let path = owned_directory.join(format!("compatible-asset-{index}.entry"));
        copy_verified(cas, pins, hash, size, role(hash), &path, cancellation)?;
        result.preserved_files += 1;
        result.preserved_bytes = result.preserved_bytes.saturating_add(size);
        if alias.kind == "inlay" {
            let ext = inlay_extension(&alias.ext);
            if ext != alias.ext {
                loss(&mut result, "converted-inlay-extension", size);
            }
            let id = if original_inlay_ids.contains(alias.key.as_str()) {
                alias.key.clone()
            } else {
                let mut index = 0u64;
                loop {
                    let value = stable_id(&format!("inlay:{}:{index}", alias.key));
                    if reserve_inlay_files(&value, &ext, &mut inlay_files) {
                        break value;
                    }
                    index += 1;
                }
            };
            if id != alias.key {
                result
                    .inlay_replacements
                    .insert(alias.key.clone(), id.clone());
                warning(&mut result, "inlay-ids-remapped");
                warning(&mut result, "opaque-plugin-inlay-references-unverified");
            }
            let kind = alias
                .inlay_type
                .as_deref()
                .filter(|k| matches!(*k, "image" | "audio" | "video" | "signature"))
                .ok_or_else(|| invalid_source("inlay has an unsupported type"))?;
            if alias.width.is_some_and(|v| v < 0) || alias.height.is_some_and(|v| v < 0) {
                return Err(invalid_source("inlay dimensions are invalid"));
            }
            if kind == "signature" {
                let reader =
                    CancellationReader::new(File::open(&path).map_err(io_job_error)?, cancellation);
                let mut decoder = serde_json::Deserializer::from_reader(BufReader::new(reader));
                serde::Deserialize::deserialize(&mut decoder)
                    .map(|_: IgnoredAny| ())
                    .map_err(|_| invalid_source("signature payload is not valid JSON"))?;
                decoder
                    .end()
                    .map_err(|_| invalid_source("signature payload has trailing data"))?;
            }
            result.entries.push(LegacyBackupWriteEntry {
                logical_name: format!("inlay/{id}.{ext}"),
                source: LegacyBackupWriteSource::File(path),
            });
            let mut sidecar = serde_json::json!({"ext":ext,"name":alias.name,"type":kind});
            for (key, dimension) in [("width", alias.width), ("height", alias.height)] {
                if let Some(value) = dimension {
                    sidecar[key] = Value::from(value);
                }
            }
            let sidecar_path = owned_directory.join(format!("compatible-sidecar-{index}.json"));
            write_json(&sidecar_path, &sidecar)?;
            loss(
                &mut result,
                "converted-inlay-sidecars",
                std::fs::metadata(&sidecar_path)
                    .map_err(io_job_error)?
                    .len(),
            );
            result.entries.push(LegacyBackupWriteEntry {
                logical_name: format!("inlay_sidecar/{id}"),
                source: LegacyBackupWriteSource::File(sidecar_path),
            });
            if let Some(value) = provenance(alias) {
                let meta_path = owned_directory.join(format!("compatible-provenance-{index}.json"));
                write_json(&meta_path, &value)?;
                loss(
                    &mut result,
                    "converted-inlay-provenance",
                    std::fs::metadata(&meta_path).map_err(io_job_error)?.len(),
                );
                result.entries.push(LegacyBackupWriteEntry {
                    logical_name: format!("inlay_meta/{id}"),
                    source: LegacyBackupWriteSource::File(meta_path),
                });
            }
            if kind != "signature" {
                loss(&mut result, "inlay-codec-playback-unverified", size);
            }
        } else if alias.kind == "asset" {
            if target == CompatibilityTarget::RisuAi && !supported_image_mime(&alias.mime) {
                loss(&mut result, "asset-playback-unverified", size);
            }
            let name = original_asset_names
                .get(alias.key.as_str())
                .cloned()
                .unwrap_or_else(|| {
                    // Preserve a nested source's basename when no flat source or
                    // earlier logical key already owns it.
                    if let Some(basename) = alias
                        .key
                        .strip_prefix("assets/")
                        .and_then(|key| key.rsplit(['/', '\\']).next())
                    {
                        if let Some(candidate) = asset_name(&format!("assets/{basename}"), target) {
                            if names.insert(name_identity(&candidate)) {
                                return candidate;
                            }
                        }
                    }
                    let extension = if target == CompatibilityTarget::RisuAi {
                        "png".to_owned()
                    } else {
                        safe_owner_extension(&alias.ext)
                    };
                    allocate_asset_name(&alias.key, &extension, &mut names)
                });
            let replacement = format!("assets/{name}");
            if replacement != alias.key {
                result.replacements.insert(alias.key.clone(), replacement);
                warning(&mut result, "asset-paths-remapped");
                warning(&mut result, "opaque-plugin-asset-references-unverified");
            }
            result.entries.push(LegacyBackupWriteEntry {
                logical_name: name,
                source: LegacyBackupWriteSource::File(path),
            });
        } else {
            return Err(invalid_source("unknown attachment alias type"));
        }
    }

    let mut payload_keys = HashMap::<(String, String), String>::new();
    let mut unverified_owner_payloads = HashSet::new();
    for head in inventory.owner_heads.iter().filter(|head| head.present) {
        check_cancelled(cancellation).map_err(local_backup_error)?;
        let hash = head
            .manifest_hash
            .as_deref()
            .ok_or_else(|| invalid_source("owner manifest hash is missing"))?;
        let size = cas
            .stat_available_object(hash)
            .map_err(io_job_error)?
            .ok_or_else(|| invalid_source("owner manifest is missing"))?;
        if size > MAX_OWNER_MANIFEST_BYTES {
            return Err(invalid_source("owner manifest exceeds the decode limit"));
        }
        pins.pin_existing(cas, hash, size, CasObjectRole::OwnerManifest)
            .map_err(io_job_error)?;
        let entries = read_owner_manifest(cas, hash, size, cancellation)?;
        if entries.len() as i64 != head.entry_count {
            return Err(invalid_source("owner manifest count mismatch"));
        }
        let mut keys = Vec::with_capacity(entries.len());
        for entry in entries {
            let hash = hex::encode(
                entry
                    .payload_hash
                    .ok_or_else(|| invalid_source("owner attachment payload is missing"))?,
            );
            let ext = if target == CompatibilityTarget::RisuAi {
                "png".to_owned()
            } else {
                safe_owner_extension(&entry.tuple[2])
            };
            let identity = (hash.clone(), ext.clone());
            let key = if let Some(key) = payload_keys.get(&identity) {
                key.clone()
            } else {
                let name = allocate_asset_name(&format!("owner:{hash}"), &ext, &mut names);
                let path =
                    owned_directory.join(format!("compatible-owner-{}.entry", payload_keys.len()));
                let size = cas
                    .stat_available_object(&hash)
                    .map_err(io_job_error)?
                    .ok_or_else(|| invalid_source("owner attachment object is missing"))?;
                copy_verified(cas, pins, &hash, size, role(&hash), &path, cancellation)?;
                result.preserved_files += 1;
                result.preserved_bytes = result.preserved_bytes.saturating_add(size);
                let key = format!("assets/{name}");
                result.entries.push(LegacyBackupWriteEntry {
                    logical_name: name,
                    source: LegacyBackupWriteSource::File(path),
                });
                payload_keys.insert(identity, key.clone());
                key
            };
            if target == CompatibilityTarget::RisuAi && !supported_image_extension(&entry.tuple[2])
            {
                result.owner_playback_unverified.insert(head.owner.clone());
            }
            if target == CompatibilityTarget::RisuAi
                && !supported_image_extension(&entry.tuple[2])
                && unverified_owner_payloads.insert(hash.clone())
            {
                let size = cas
                    .stat_available_object(&hash)
                    .map_err(io_job_error)?
                    .ok_or_else(|| invalid_source("owner attachment object is missing"))?;
                loss(&mut result, "asset-playback-unverified", size);
            }
            keys.push(key);
        }
        result
            .owner_replacement_keys
            .insert(head.owner.clone(), keys);
    }
    Ok(result)
}

type ImpactCodes = HashSet<String>;

#[derive(Default)]
struct AttachmentImpactIndex {
    assets: HashMap<String, ImpactCodes>,
    inlays: HashMap<String, ImpactCodes>,
    provenance: HashMap<(String, String), ImpactCodes>,
}

fn impact_reference(index: &mut HashMap<String, ImpactCodes>, key: &str, code: &str) {
    index
        .entry(key.to_owned())
        .or_default()
        .insert(code.to_owned());
}

impl AttachmentImpactIndex {
    fn opaque_assets(&self, value: &Value, found: &mut ImpactCodes) {
        match value {
            Value::String(_) => Self::exact(&self.assets, value, found),
            Value::Array(items) => {
                for item in items {
                    self.opaque_assets(item, found);
                }
            }
            Value::Object(items) => {
                for item in items.values() {
                    self.opaque_assets(item, found);
                }
            }
            _ => {}
        }
    }
    fn exact(index: &HashMap<String, ImpactCodes>, value: &Value, found: &mut ImpactCodes) {
        if let Some(codes) = value.as_str().and_then(|key| index.get(key)) {
            found.extend(codes.iter().cloned());
        }
    }

    fn assets(&self, value: &Value, found: &mut ImpactCodes) {
        match value {
            Value::Array(items) => {
                for item in items {
                    self.assets(item, found);
                }
            }
            Value::Object(object) => {
                for (key, item) in object {
                    match key.as_str() {
                        "image" | "icon" | "customBackground" | "userIcon" | "imgFile" | "img" => {
                            Self::exact(&self.assets, item, found)
                        }
                        "additionalAssets" | "emotionImages" | "assets" => {
                            if let Some(items) = item.as_array() {
                                for tuple in items {
                                    if let Some(value) = tuple.get(1) {
                                        Self::exact(&self.assets, value, found);
                                    }
                                }
                            }
                        }
                        "ccAssets" => {
                            if let Some(items) = item.as_array() {
                                for asset in items {
                                    if let Some(value) = asset.get("uri") {
                                        Self::exact(&self.assets, value, found);
                                    }
                                }
                            }
                        }
                        "vits" => {
                            if let Some(files) = item.get("files").and_then(Value::as_object) {
                                for value in files.values() {
                                    Self::exact(&self.assets, value, found);
                                }
                            }
                        }
                        "modules" | "personas" | "embeddedModule" | "characterOrder" => {
                            self.assets(item, found)
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn text(
        &self,
        value: &Value,
        found: &mut ImpactCodes,
        visiting: &mut HashSet<String>,
        cancel: &dyn CancellationProbe,
    ) -> Result<(), NativeJobError> {
        match value {
            Value::String(text) => {
                // Scan the exact inlay grammar, never arbitrary ID substrings.
                for fragment in text.split("{{").skip(1) {
                    let Some(token) = fragment.split_once("}}").map(|part| part.0) else {
                        continue;
                    };
                    let Some((kind, id)) = token.split_once("::") else {
                        continue;
                    };
                    if matches!(kind, "inlay" | "inlayed" | "inlayeddata") {
                        if let Some(codes) = self.inlays.get(id) {
                            found.extend(codes.iter().cloned());
                        }
                    }
                }
            }
            Value::Array(items) => {
                for item in items {
                    self.text(item, found, visiting, cancel)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn value(
        &self,
        value: &Value,
        found: &mut ImpactCodes,
        visiting: &mut HashSet<String>,
        cancel: &dyn CancellationProbe,
    ) -> Result<(), NativeJobError> {
        check_cancelled(cancel).map_err(local_backup_error)?;
        self.assets(value, found);
        match value {
            Value::Array(items) => {
                for item in items {
                    self.value(item, found, visiting, cancel)?;
                }
            }
            Value::Object(object) => {
                // These are the app's structural message/conversation boundaries.
                for key in [
                    "message",
                    "messages",
                    "character",
                    "chats",
                    "responseVariants",
                    "candidates",
                    "rerollRecovery",
                    "original",
                    "outputs",
                ] {
                    if let Some(item) = object.get(key) {
                        if key == "outputs" {
                            if let Some(outputs) = item.as_object() {
                                for item in outputs.values() {
                                    self.value(item, found, visiting, cancel)?;
                                }
                            }
                        } else {
                            self.value(item, found, visiting, cancel)?;
                        }
                    }
                }
                if object.contains_key("role") {
                    for key in ["data", "swipes"] {
                        if let Some(item) = object.get(key) {
                            self.text(item, found, visiting, cancel)?;
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Counts identities from the same leased source as the exporter. Registered but
/// unreferenced attachments prove zero; opaque plugin ownership stays unknown.
pub(super) fn count_affected_conversations(
    prepared: &mut PreparedAttachments,
    connection: &rusqlite::Connection,
    target: &crate::persistent_store::ReadTarget,
    inventory: &export::PinnedLegacyBackupInventory,
    cancel: &dyn CancellationProbe,
) -> Result<(), NativeJobError> {
    let mut index = AttachmentImpactIndex::default();
    let excluded_inlays = prepared
        .warning_codes
        .iter()
        .any(|c| c == "risuai-inlays-excluded");
    for alias in &inventory.assets {
        if alias.kind == "asset" {
            if prepared.replacements.contains_key(&alias.key) {
                impact_reference(&mut index.assets, &alias.key, "asset-paths-remapped");
            }
            if excluded_inlays
                || prepared
                    .losses
                    .iter()
                    .any(|item| item.0 == "asset-playback-unverified")
            {
                if !supported_image_mime(&alias.mime) {
                    impact_reference(&mut index.assets, &alias.key, "asset-playback-unverified");
                }
            }
        } else if alias.kind == "inlay" {
            if excluded_inlays {
                impact_reference(&mut index.inlays, &alias.key, "risuai-inlays-excluded");
            } else {
                impact_reference(&mut index.inlays, &alias.key, "converted-inlay-sidecars");
                if provenance(alias).is_some() {
                    impact_reference(&mut index.inlays, &alias.key, "converted-inlay-provenance");
                }
                if alias.inlay_type.as_deref() != Some("signature") {
                    impact_reference(
                        &mut index.inlays,
                        &alias.key,
                        "inlay-codec-playback-unverified",
                    );
                }
                if inlay_extension(&alias.ext) != alias.ext {
                    impact_reference(&mut index.inlays, &alias.key, "converted-inlay-extension");
                }
                if prepared.inlay_replacements.contains_key(&alias.key) {
                    impact_reference(&mut index.inlays, &alias.key, "inlay-ids-remapped");
                }
            }
        }
    }
    for alias in inventory
        .assets
        .iter()
        .filter(|alias| alias.kind == "inlay")
    {
        let metadata = alias.metadata.get("pocketRisu");
        if let (Some(character), Some(chat), Some(codes)) = (
            metadata
                .and_then(|value| value.get("charId"))
                .and_then(Value::as_str),
            metadata
                .and_then(|value| value.get("chatId"))
                .and_then(Value::as_str),
            index.inlays.get(&alias.key),
        ) {
            index
                .provenance
                .entry((character.to_owned(), chat.to_owned()))
                .or_default()
                .extend(codes.iter().cloned());
        }
    }
    let mut known_codes: ImpactCodes = index
        .assets
        .values()
        .chain(index.inlays.values())
        .flat_map(|codes| codes.iter().cloned())
        .collect();
    if !prepared.owner_replacement_keys.is_empty() {
        known_codes.insert("owner-asset-arrays-rehydrated".to_owned());
    }
    if !prepared.owner_playback_unverified.is_empty() {
        known_codes.insert("asset-playback-unverified".to_owned());
    }
    let mut affected: HashMap<String, HashSet<(String, String)>> = known_codes
        .iter()
        .map(|code| (code.clone(), HashSet::new()))
        .collect();
    let sql_error = |error| store_job_error(crate::persistent_store::StoreError::from(error));
    let parse_json = |text: String| {
        serde_json::from_str::<Value>(&text)
            .map_err(|_| invalid_source("snapshot record JSON is invalid"))
    };
    let mut root = parse_json(
        connection
            .query_row(
                "SELECT value FROM root WHERE generation=?1",
                [&target.generation],
                |row| row.get(0),
            )
            .map_err(sql_error)?,
    )?;
    let mut global = ImpactCodes::new();
    // Module/persona ownership depends on runtime activation and cannot be inferred
    // from an owner inventory alone. Avoid a fabricated count for these categories.
    let mut unknown = ImpactCodes::new();
    for key in ["plugins", "pluginCustomStorage"] {
        if let Some(value) = root.get(key) {
            index.opaque_assets(value, &mut unknown);
        }
    }
    if let Some(root) = root.as_object_mut() {
        for key in ["modules", "personas"] {
            if let Some(scoped) = root.remove(key) {
                index.assets(&scoped, &mut unknown);
            }
        }
    }
    index.assets(&root, &mut global);
    let mut presets = connection
        .prepare("SELECT value FROM bot_presets WHERE generation=?1")
        .map_err(sql_error)?;
    let mut preset_rows = presets.query([&target.generation]).map_err(sql_error)?;
    while let Some(row) = preset_rows.next().map_err(sql_error)? {
        check_cancelled(cancel).map_err(local_backup_error)?;
        index.assets(&parse_json(row.get(0).map_err(sql_error)?)?, &mut unknown);
    }
    let mut plugin_storage = connection
        .prepare("SELECT value FROM plugin_storage WHERE generation=?1")
        .map_err(sql_error)?;
    let mut plugin_rows = plugin_storage
        .query([&target.generation])
        .map_err(sql_error)?;
    while let Some(row) = plugin_rows.next().map_err(sql_error)? {
        check_cancelled(cancel).map_err(local_backup_error)?;
        index.opaque_assets(&parse_json(row.get(0).map_err(sql_error)?)?, &mut unknown);
    }
    for owner in prepared.owner_replacement_keys.keys() {
        if !matches!(owner, AssetOwnerLocator::CharacterAdditionalAssets { .. }) {
            unknown.insert("owner-asset-arrays-rehydrated".to_owned());
        }
    }
    for owner in &prepared.owner_playback_unverified {
        if !matches!(owner, AssetOwnerLocator::CharacterAdditionalAssets { .. }) {
            unknown.insert("asset-playback-unverified".to_owned());
        }
    }
    for code in &prepared.warning_codes {
        if code.starts_with("opaque-plugin-") {
            unknown.insert(code.clone());
        }
    }
    let mut characters = connection.prepare("SELECT character_id,detail FROM characters WHERE generation=?1 ORDER BY configured_index").map_err(sql_error)?;
    let mut rows = characters.query([&target.generation]).map_err(sql_error)?;
    while let Some(row) = rows.next().map_err(sql_error)? {
        check_cancelled(cancel).map_err(local_backup_error)?;
        let character_id: String = row.get(0).map_err(sql_error)?;
        let character = parse_json(row.get(1).map_err(sql_error)?)?;
        let mut shared = global.clone();
        let owner = AssetOwnerLocator::CharacterAdditionalAssets {
            character_id: character_id.clone(),
        };
        if prepared.owner_replacement_keys.contains_key(&owner) {
            shared.insert("owner-asset-arrays-rehydrated".to_owned());
        }
        if prepared.owner_playback_unverified.contains(&owner) {
            shared.insert("asset-playback-unverified".to_owned());
        }
        index.assets(&character, &mut shared);
        let mut record = |conversation_id: String, codes: ImpactCodes| {
            for code in codes {
                affected
                    .entry(code)
                    .or_default()
                    .insert((character_id.clone(), conversation_id.clone()));
            }
        };
        let mut statement = connection.prepare("SELECT conversation_id,detail FROM conversations WHERE generation=?1 AND character_id=?2 ORDER BY configured_index").map_err(sql_error)?;
        let mut chats = statement
            .query(rusqlite::params![&target.generation, &character_id])
            .map_err(sql_error)?;
        while let Some(row) = chats.next().map_err(sql_error)? {
            let conversation_id: String = row.get(0).map_err(sql_error)?;
            let mut found = shared.clone();
            if let Some(codes) = index
                .provenance
                .get(&(character_id.clone(), conversation_id.clone()))
            {
                found.extend(codes.iter().cloned());
            }
            index.value(
                &parse_json(row.get(1).map_err(sql_error)?)?,
                &mut found,
                &mut HashSet::new(),
                cancel,
            )?;
            let mut statement = connection.prepare("SELECT value FROM messages WHERE generation=?1 AND character_id=?2 AND conversation_id=?3 ORDER BY message_index").map_err(sql_error)?;
            let mut messages = statement
                .query(rusqlite::params![
                    &target.generation,
                    &character_id,
                    &conversation_id
                ])
                .map_err(sql_error)?;
            while let Some(row) = messages.next().map_err(sql_error)? {
                index.value(
                    &parse_json(row.get(0).map_err(sql_error)?)?,
                    &mut found,
                    &mut HashSet::new(),
                    cancel,
                )?;
            }
            record(conversation_id, found);
        }
    }
    for (code, identities) in affected {
        prepared
            .affected_conversations
            .insert(code, Some(identities.len() as u64));
    }
    for code in unknown {
        prepared.affected_conversations.insert(code, None);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synthetic_alias(cas: &PayloadCas, key: &str, kind: &str, bytes: &[u8]) -> AssetAlias {
        let payload = cas.prepare_bytes(bytes).unwrap();
        AssetAlias {
            key: key.to_owned(),
            object_hash: Some(payload.content_hash),
            kind: kind.to_owned(),
            size: payload.byte_size as i64,
            mime: "image/png".to_owned(),
            name: "synthetic.png".to_owned(),
            ext: "png".to_owned(),
            inlay_type: (kind == "inlay").then(|| "image".to_owned()),
            width: Some(1),
            height: Some(1),
            metadata: serde_json::json!({"pocketRisu":{"createdAt":123,"charId":"synthetic-character","unsupported":"discard"}}),
        }
    }

    fn read_entry(result: &PreparedAttachments, name: &str) -> Vec<u8> {
        let entry = result
            .entries
            .iter()
            .find(|e| e.logical_name == name)
            .unwrap();
        let LegacyBackupWriteSource::File(path) = &entry.source else {
            panic!("file expected");
        };
        std::fs::read(path).unwrap()
    }

    #[test]
    fn target_attachments_preserve_bytes_expand_owners_and_exclude_risuai_inlays() {
        let root = std::env::temp_dir().join(format!("compatible-assets-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("repository")).unwrap();
        let cas = PayloadCas::new(root.join("repository")).unwrap();
        let alias = synthetic_alias(&cas, "assets/flat.png", "asset", b"synthetic first");
        let mut nested =
            synthetic_alias(&cas, "assets/nested/flat.png", "asset", b"synthetic second");
        nested.mime = "audio/mpeg".to_owned();
        let inlay = synthetic_alias(&cas, "bad/inlay", "inlay", b"synthetic image");
        let owner = AssetOwnerLocator::CharacterAdditionalAssets {
            character_id: "synthetic-character".to_owned(),
        };
        let mut payload_hash = [0u8; 32];
        payload_hash.copy_from_slice(&hex::decode(alias.object_hash.as_ref().unwrap()).unwrap());
        let manifest = owner_manifest_codec::encode_owner_manifest(&[
            owner_manifest_codec::OwnerManifestEntry {
                tuple: [
                    "synthetic asset".to_owned(),
                    "assets/prior.png".to_owned(),
                    "mp3".to_owned(),
                ],
                payload_hash: Some(payload_hash),
            },
        ])
        .unwrap();
        let manifest = cas.prepare_bytes(&manifest).unwrap();
        let inventory = export::PinnedLegacyBackupInventory {
            revision: 1,
            assets: vec![alias, nested, inlay],
            owner_heads: vec![AssetOwnerHead {
                owner: owner.clone(),
                present: true,
                manifest_hash: Some(manifest.content_hash),
                entry_count: 1,
            }],
            asset_authority: AssetRepositoryAuthorityState::Legacy,
        };
        for target in [CompatibilityTarget::RisuAi, CompatibilityTarget::PocketRisu] {
            let output = root.join(if target == CompatibilityTarget::RisuAi {
                "risu"
            } else {
                "pocket"
            });
            std::fs::create_dir(&output).unwrap();
            let mut pins = DurableCasJob::begin(
                &root.join("repository"),
                &Uuid::new_v4().to_string(),
                CasJobKind::OfficialPublicationOrExportPreparation,
                0,
            )
            .unwrap();
            let result = prepare(
                target,
                &output,
                &cas,
                &inventory,
                &mut pins,
                &crate::local_backup::NeverCancelled,
            )
            .unwrap();
            assert_eq!(read_entry(&result, "flat.png"), b"synthetic first");
            let mapped = result.replacements.get("assets/nested/flat.png").unwrap();
            assert_eq!(
                read_entry(&result, mapped.strip_prefix("assets/").unwrap()),
                b"synthetic second"
            );
            let owner_key = &result.owner_replacement_keys[&owner][0];
            assert_eq!(
                read_entry(&result, owner_key.strip_prefix("assets/").unwrap()),
                b"synthetic first"
            );
            if target == CompatibilityTarget::RisuAi {
                assert!(result
                    .entries
                    .iter()
                    .all(|e| e.logical_name.ends_with(".png") && !e.logical_name.contains('/')));
                assert_eq!(
                    result.losses,
                    vec![
                        ("asset-playback-unverified".to_owned(), 2, 31),
                        ("risuai-inlays-excluded".to_owned(), 1, 15),
                    ]
                );
            } else {
                let id = &result.inlay_replacements["bad/inlay"];
                assert_eq!(
                    read_entry(&result, &format!("inlay/{id}.png")),
                    b"synthetic image"
                );
                let sidecar: Value =
                    serde_json::from_slice(&read_entry(&result, &format!("inlay_sidecar/{id}")))
                        .unwrap();
                assert_eq!(
                    sidecar,
                    serde_json::json!({"ext":"png","name":"synthetic.png","type":"image","width":1,"height":1})
                );
                let meta: Value =
                    serde_json::from_slice(&read_entry(&result, &format!("inlay_meta/{id}")))
                        .unwrap();
                assert_eq!(
                    meta,
                    serde_json::json!({"createdAt":123,"charId":"synthetic-character"})
                );
                assert!(!result
                    .entries
                    .iter()
                    .any(|e| e.logical_name.ends_with(".risuinlay")
                        || e.logical_name.starts_with("inlay_info/")
                        || e.logical_name.starts_with("inlay_thumb/")));
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn generated_names_cannot_steal_original_asset_names_or_inlay_ids() {
        let root = std::env::temp_dir().join(format!("compatible-assets-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("repository")).unwrap();
        let cas = PayloadCas::new(root.join("repository")).unwrap();
        let reserved_name = format!("asset-{}.png", stable_id("assets/nested/flat.png:0"));
        let reserved_id = stable_id("inlay:bad/inlay:0");
        let inventory = export::PinnedLegacyBackupInventory {
            revision: 1,
            assets: vec![
                synthetic_alias(&cas, "assets/flat.png", "asset", b"flat"),
                synthetic_alias(&cas, "assets/nested/flat.png", "asset", b"nested"),
                synthetic_alias(
                    &cas,
                    &format!("assets/{reserved_name}"),
                    "asset",
                    b"reserved asset",
                ),
                synthetic_alias(&cas, "bad/inlay", "inlay", b"remapped inlay"),
                synthetic_alias(&cas, &reserved_id, "inlay", b"reserved inlay"),
            ],
            owner_heads: vec![],
            asset_authority: AssetRepositoryAuthorityState::Legacy,
        };
        let mut pins = DurableCasJob::begin(
            &root.join("repository"),
            &Uuid::new_v4().to_string(),
            CasJobKind::OfficialPublicationOrExportPreparation,
            0,
        )
        .unwrap();
        let result = prepare(
            CompatibilityTarget::PocketRisu,
            &root,
            &cas,
            &inventory,
            &mut pins,
            &crate::local_backup::NeverCancelled,
        )
        .unwrap();
        assert_eq!(read_entry(&result, &reserved_name), b"reserved asset");
        let mapped_asset = result.replacements["assets/nested/flat.png"]
            .strip_prefix("assets/")
            .unwrap();
        assert_ne!(mapped_asset, reserved_name);
        assert_eq!(read_entry(&result, mapped_asset), b"nested");
        let mapped_id = &result.inlay_replacements["bad/inlay"];
        assert_ne!(mapped_id, &reserved_id);
        assert_eq!(
            read_entry(&result, &format!("inlay/{mapped_id}.png")),
            b"remapped inlay"
        );
        assert_eq!(
            read_entry(&result, &format!("inlay/{reserved_id}.png")),
            b"reserved inlay"
        );
        assert_eq!(
            result
                .entries
                .iter()
                .map(|e| &e.logical_name)
                .collect::<HashSet<_>>()
                .len(),
            result.entries.len()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn windows_names_and_case_collisions_keep_distinct_payload_references() {
        let root = std::env::temp_dir().join(format!("compatible-assets-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("repository")).unwrap();
        let cas = PayloadCas::new(root.join("repository")).unwrap();
        let mut assets: Vec<_> = [
            "assets/Test.png",
            "assets/test.png",
            "assets/CON.png",
            "assets/a:b.png",
            "assets/trailing.png ",
        ]
        .iter()
        .map(|key| synthetic_alias(&cas, key, "asset", key.as_bytes()))
        .collect();
        for id in [
            "Clip",
            "clip",
            "NUL",
            "trailing.",
            "item",
            "item.meta",
            "custom",
        ] {
            let mut alias = synthetic_alias(&cas, id, "inlay", id.as_bytes());
            if id == "item.meta" {
                alias.ext = "json".to_owned();
            }
            if id == "custom" {
                alias.ext = "mp:3".to_owned();
            }
            assets.push(alias);
        }
        let inventory = export::PinnedLegacyBackupInventory {
            revision: 1,
            assets,
            owner_heads: vec![],
            asset_authority: AssetRepositoryAuthorityState::Legacy,
        };
        for target in [CompatibilityTarget::RisuAi, CompatibilityTarget::PocketRisu] {
            let output = root.join(if target == CompatibilityTarget::RisuAi {
                "risu"
            } else {
                "pocket"
            });
            std::fs::create_dir(&output).unwrap();
            let mut pins = DurableCasJob::begin(
                &root.join("repository"),
                &Uuid::new_v4().to_string(),
                CasJobKind::OfficialPublicationOrExportPreparation,
                0,
            )
            .unwrap();
            let result = prepare(
                target,
                &output,
                &cas,
                &inventory,
                &mut pins,
                &crate::local_backup::NeverCancelled,
            )
            .unwrap();
            assert!(!result.replacements.contains_key("assets/Test.png"));
            assert!(result.replacements.contains_key("assets/test.png"));
            let mut flat_names = HashSet::new();
            let imported_assets = output.join("imported-assets");
            let imported_inlays = output.join("imported-inlays");
            std::fs::create_dir(&imported_assets).unwrap();
            std::fs::create_dir(&imported_inlays).unwrap();
            for alias in inventory.assets.iter().filter(|a| a.kind == "asset") {
                let mapped = result
                    .replacements
                    .get(&alias.key)
                    .unwrap_or(&alias.key)
                    .strip_prefix("assets/")
                    .unwrap();
                assert!(safe_component(mapped));
                assert!(flat_names.insert(name_identity(mapped)));
                assert_eq!(read_entry(&result, mapped), alias.key.as_bytes());
                std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(imported_assets.join(mapped))
                    .unwrap();
            }
            if target == CompatibilityTarget::PocketRisu {
                assert!(!result.inlay_replacements.contains_key("Clip"));
                for id in ["clip", "NUL", "trailing.", "item.meta"] {
                    assert!(result.inlay_replacements.contains_key(id));
                }
                let mut files = HashSet::new();
                for alias in inventory.assets.iter().filter(|a| a.kind == "inlay") {
                    let id = result
                        .inlay_replacements
                        .get(&alias.key)
                        .unwrap_or(&alias.key);
                    let ext = inlay_extension(&alias.ext);
                    assert!(reserve_inlay_files(id, &ext, &mut files));
                    for name in [format!("{id}.{ext}"), format!("{id}.meta.json")] {
                        std::fs::OpenOptions::new()
                            .create_new(true)
                            .write(true)
                            .open(imported_inlays.join(name))
                            .unwrap();
                    }
                    assert_eq!(
                        read_entry(&result, &format!("inlay/{id}.{ext}")),
                        alias.key.as_bytes()
                    );
                    assert!(result
                        .entries
                        .iter()
                        .any(|e| e.logical_name == format!("inlay_sidecar/{id}")));
                }
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn attachment_impact_counts_leased_conversations_and_true_zero() {
        let root = std::env::temp_dir().join(format!("compatible-impact-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("repository")).unwrap();
        let cas = PayloadCas::new(root.join("repository")).unwrap();
        let mut asset = synthetic_alias(&cas, "assets/bad.png", "asset", b"synthetic asset");
        asset.mime = "audio/mpeg".to_owned();
        let inlay = synthetic_alias(&cas, "bad/id", "inlay", b"synthetic inlay");
        let mut unused = synthetic_alias(&cas, "unused", "inlay", b"unused inlay");
        unused.ext = "mp:3".to_owned();
        let inventory = export::PinnedLegacyBackupInventory {
            revision: 1,
            assets: vec![asset, inlay, unused],
            owner_heads: vec![],
            asset_authority: AssetRepositoryAuthorityState::Legacy,
        };
        let mut prepared = PreparedAttachments::default();
        prepared
            .replacements
            .insert("assets/bad.png".into(), "assets/mapped.png".into());
        prepared
            .inlay_replacements
            .insert("bad/id".into(), "mapped".into());
        prepared
            .warning_codes
            .push("opaque-plugin-inlay-references-unverified".into());
        prepared
            .losses
            .push(("asset-playback-unverified".into(), 1, 15));
        let owner = AssetOwnerLocator::CharacterAdditionalAssets {
            character_id: "c3".into(),
        };
        prepared
            .owner_replacement_keys
            .insert(owner.clone(), vec!["assets/owner.png".into()]);
        prepared.owner_playback_unverified.insert(owner);
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection.execute_batch("CREATE TABLE root(generation TEXT,value TEXT); CREATE TABLE bot_presets(generation TEXT,value TEXT); CREATE TABLE plugin_storage(generation TEXT,value TEXT); CREATE TABLE characters(generation TEXT,character_id TEXT,detail TEXT,configured_index INTEGER); CREATE TABLE conversations(generation TEXT,character_id TEXT,conversation_id TEXT,detail TEXT,configured_index INTEGER); CREATE TABLE messages(generation TEXT,character_id TEXT,conversation_id TEXT,value TEXT,message_index INTEGER);").unwrap();
        connection
            .execute("INSERT INTO root VALUES('leased','{}')", [])
            .unwrap();
        for (id, detail) in [
            ("c1", serde_json::json!({"image":"assets/bad.png"})),
            ("c3", serde_json::json!({})),
        ] {
            connection
                .execute(
                    "INSERT INTO characters VALUES('leased',?1,?2,0)",
                    rusqlite::params![id, detail.to_string()],
                )
                .unwrap();
        }
        for (character, chat, messages) in [
            (
                "c1",
                "a",
                serde_json::json!([{"role":"char","data":"{{inlay::bad/id}}"},{"role":"char","data":"{{inlay::bad/id}}"}]),
            ),
            (
                "c1",
                "b",
                serde_json::json!([{"role":"char","data":"\u{ef01}COLDSTORAGE\u{ef01}outer-raw"}]),
            ),
            (
                "c3",
                "a",
                serde_json::json!([{"role":"char","data":"bare bad/id and unused are not references","responseVariants":{"candidates":[{"messages":[{"role":"char","data":"{{inlay::bad/id}}"}]}]}}]),
            ),
        ] {
            connection
                .execute(
                    "INSERT INTO conversations VALUES('leased',?1,?2,'{}',0)",
                    rusqlite::params![character, chat],
                )
                .unwrap();
            for (ordinal, message) in messages.as_array().unwrap().iter().enumerate() {
                connection
                    .execute(
                        "INSERT INTO messages VALUES('leased',?1,?2,?3,?4)",
                        rusqlite::params![character, chat, message.to_string(), ordinal as i64],
                    )
                    .unwrap();
            }
        }
        let target = crate::persistent_store::ReadTarget {
            revision: 1,
            generation: "leased".into(),
        };
        count_affected_conversations(
            &mut prepared,
            &connection,
            &target,
            &inventory,
            &crate::local_backup::NeverCancelled,
        )
        .unwrap();
        for code in [
            "inlay-ids-remapped",
            "inlay-codec-playback-unverified",
            "converted-inlay-sidecars",
            "converted-inlay-provenance",
        ] {
            assert_eq!(
                prepared.affected_conversations[code],
                Some(2),
                "unexpected synthetic count category"
            );
        }
        assert_eq!(
            prepared.affected_conversations["converted-inlay-extension"],
            Some(0)
        );
        assert_eq!(
            prepared.affected_conversations["asset-paths-remapped"],
            Some(2)
        );
        assert_eq!(
            prepared.affected_conversations["asset-playback-unverified"],
            Some(3)
        );
        assert_eq!(
            prepared.affected_conversations["owner-asset-arrays-rehydrated"],
            Some(1)
        );
        assert_eq!(
            prepared.affected_conversations["opaque-plugin-inlay-references-unverified"],
            None
        );
        connection
            .execute(
                "INSERT INTO bot_presets VALUES('leased',?1)",
                [serde_json::json!({"image":"assets/bad.png"}).to_string()],
            )
            .unwrap();
        count_affected_conversations(
            &mut prepared,
            &connection,
            &target,
            &inventory,
            &crate::local_backup::NeverCancelled,
        )
        .unwrap();
        assert_eq!(
            prepared.affected_conversations["asset-paths-remapped"],
            None
        );
        assert_eq!(
            prepared.affected_conversations["asset-playback-unverified"],
            None
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn names_and_extensions_match_target_rules() {
        assert_eq!(
            asset_name("assets/a.png", CompatibilityTarget::RisuAi).as_deref(),
            Some("a.png")
        );
        for key in [
            "assets/a.jpg",
            "assets/a/b.png",
            "assets/database.risudat",
            "assets/coldstorage_x.json",
        ] {
            assert!(asset_name(key, CompatibilityTarget::RisuAi).is_none());
        }
        assert_eq!(normalized_extension(" ..MP/3\\\0 "), "mp3");
        assert_eq!(normalized_extension("..."), "bin");
        assert_eq!(inlay_extension("custom.codec"), "bin");
        assert!(!safe_inlay_id("..metadata"));
        for name in [
            "CON",
            "nul.png",
            "AUX.jpeg",
            "COM1.txt",
            "LPT9",
            "COM¹.png",
            "trailing.",
            "trailing ",
            "bad:name",
            "a?b",
            "a\u{0001}b",
        ] {
            assert!(!safe_component(name), "unsafe synthetic name accepted");
        }
        assert!(safe_component("console.png"));
        assert!(safe_component("COM10.png"));
        assert!(supported_image_mime("image/png; charset=binary"));
        assert!(!supported_image_mime("audio/mpeg"));
        assert!(supported_image_extension("JPEG"));
        assert!(!supported_image_extension("mp3"));
        let mut names = HashSet::new();
        assert_ne!(
            allocate_asset_name("a", "png", &mut names),
            allocate_asset_name("a", "png", &mut names)
        );
    }

    #[test]
    fn copied_payload_hash_is_verified_and_source_is_isolated() {
        let root = std::env::temp_dir().join(format!("compatible-assets-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("repository")).unwrap();
        let cas = PayloadCas::new(root.join("repository")).unwrap();
        let payload = cas.prepare_bytes(b"synthetic attachment").unwrap();
        let mut pins = DurableCasJob::begin(
            &root.join("repository"),
            &Uuid::new_v4().to_string(),
            CasJobKind::OfficialPublicationOrExportPreparation,
            0,
        )
        .unwrap();
        let path = root.join("copy");
        assert!(copy_verified(
            &cas,
            &mut pins,
            &payload.content_hash,
            u64::from(u32::MAX) + 1,
            CasObjectRole::DirectObject,
            &root.join("overflow"),
            &crate::local_backup::NeverCancelled,
        )
        .is_err());
        assert!(!root.join("overflow").exists());
        assert!(copy_verified(
            &cas,
            &mut pins,
            &"0".repeat(64),
            1,
            CasObjectRole::DirectObject,
            &root.join("missing"),
            &crate::local_backup::NeverCancelled,
        )
        .is_err());
        assert!(!root.join("missing").exists());
        copy_verified(
            &cas,
            &mut pins,
            &payload.content_hash,
            payload.byte_size,
            CasObjectRole::DirectObject,
            &path,
            &crate::local_backup::NeverCancelled,
        )
        .unwrap();
        let original = cas.object_path(&payload.content_hash).unwrap().unwrap();
        std::fs::write(&original, vec![0; payload.byte_size as usize]).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"synthetic attachment");
        assert!(copy_verified(
            &cas,
            &mut pins,
            &payload.content_hash,
            payload.byte_size,
            CasObjectRole::DirectObject,
            &root.join("corrupt"),
            &crate::local_backup::NeverCancelled
        )
        .is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
