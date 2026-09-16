//! Strict foreign-app export. The authoritative detached SQL snapshot is read
//! one record at a time; no complete database is reconstructed in the WebView.
use super::compatible_projection::{
    pocket_swipes, rewrite_assets, rewrite_plugin, Projector,
};
use super::*;
use crate::persistent_store::owner_projection::OwnerManifestProjector;
use crate::persistent_store::{ReadTarget, StoreError};
use rusqlite::{params, Connection};
use std::path::Path;
const COMPRESSED_MAGIC: &[u8] = b"\0RISUSAVE\0\x08";

pub(crate) fn export_compatible_local_backup(
    target: CompatibilityTarget,
    destination_path: Option<&Path>,
    expected_revision: i64,
    owned_directory: &Path,
    handoff_directory: &Path,
    mut store: PersistentStore,
    job: &JobControl,
) -> Result<JobResultSummary, NativeJobError> {
    job.start(JobPhase::WritingExport)
        .map_err(job_state_error)?;
    let cancellation = JobCancellation(job);
    let repository_root = store.repository_root().to_path_buf();
    let cas = PayloadCas::new(&repository_root).map_err(io_job_error)?;
    let mut prepared = store
        .prepare_risu_save_export(expected_revision)
        .map_err(store_job_error)?;
    let reader = prepared.take_reader().map_err(store_job_error)?;
    let mut durable = None;
    let outcome = (|| {
        let inventory = export::pinned_legacy_backup_inventory(&reader.connection, &reader.target)
            .map_err(store_job_error)?;
        if inventory.revision != expected_revision {
            return Err(NativeJobError::new(
                "revision-conflict",
                "compatibility snapshot revision changed",
            ));
        }
        if !matches!(
            inventory.asset_authority,
            AssetRepositoryAuthorityState::V2 { .. }
        ) {
            return Err(NativeJobError::new(
                "capability-unavailable",
                "compatibility export requires a migrated attachment repository",
            ));
        }
        durable = Some(
            DurableCasJob::begin(
                &repository_root,
                &job.id(),
                CasJobKind::OfficialPublicationOrExportPreparation,
                now_millis(),
            )
            .map_err(io_job_error)?,
        );
        let pins = durable.as_mut().unwrap();
        let mut attachments = compatible_assets::prepare(
            target,
            owned_directory,
            &cas,
            &inventory,
            pins,
            &cancellation,
        )?;
        pins.seal(&mut store, now_millis()).map_err(io_job_error)?;
        compatible_assets::count_affected_conversations(
            &mut attachments,
            &reader.connection,
            &reader.target,
            &inventory,
            &cancellation,
        )?;
        let owner = OwnerManifestProjector::from_snapshots_dir_with_replacement_keys(
            &reader.connection,
            &reader.target,
            &prepared.snapshots_dir,
            std::mem::take(&mut attachments.owner_replacement_keys),
        )
        .map_err(store_job_error)?;
        let mut projector = Projector::new(target)?;
        let reference_root = parse(
            reader
                .connection
                .query_row(
                    "SELECT value FROM root WHERE generation=?1",
                    [&reader.target.generation],
                    |row| row.get(0),
                )
                .map_err(sql)?,
        )?;
        projector.set_personas(&reference_root)?;
        if target == CompatibilityTarget::PocketRisu {
            let mut statement = reader
                .connection
                .prepare("SELECT value FROM bot_presets WHERE generation=?1")
                .map_err(sql)?;
            let mut rows = statement.query([&reader.target.generation]).map_err(sql)?;
            let mut names = HashSet::new();
            while let Some(row) = rows.next().map_err(sql)? {
                if let Some(name) = parse(row.get(0).map_err(sql)?)?
                    .get("name")
                    .and_then(Value::as_str)
                {
                    names.insert(name.to_owned());
                }
            }
            if let Some(toggles) = reference_root
                .get("togglePresets")
                .and_then(Value::as_array)
            {
                for preset in toggles {
                    if let Some(name) = preset
                        .get("promptPresetName")
                        .and_then(Value::as_str)
                        .filter(|name| !name.is_empty())
                    {
                        if !names.contains(name) {
                            // Pocket displays this as historical metadata and still
                            // applies the toggle values after its mismatch prompt.
                            projector
                                .losses
                                .add("toggle-preset-prompt-name-unmatched", 1);
                        }
                    }
                }
            }
        }
        projector.set_inventory(
            attachments
                .entries
                .iter()
                .map(|entry| entry.logical_name.clone())
                .collect(),
        );
        let database = owned_directory.join("compatible-database.risudat");
        let counts = write_database(
            &reader.connection,
            &reader.target,
            &owner,
            &mut projector,
            &attachments.replacements,
            &attachments.inlay_replacements,
            &database,
            &cancellation,
        )?;
        validate_database(&database, &cancellation)?;
        attachments.entries.push(LegacyBackupWriteEntry {
            logical_name: DATABASE_ENTRY.into(),
            source: LegacyBackupWriteSource::File(database),
        });
        let archive_path = owned_directory.join(ARCHIVE_FILE);
        let mut archive = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&archive_path)
            .map_err(io_job_error)?;
        let written =
            write_legacy_local_backup_v1(&mut archive, &attachments.entries, &cancellation)
                .map_err(local_backup_error)?;
        if !written.is_complete() {
            return Err(NativeJobError::new(
                "invalid-source",
                "compatibility archive contains missing payloads",
            ));
        }
        archive.sync_all().map_err(io_job_error)?;
        let mut report = CompatibilityReport {
            target,
            preserved: vec![
                report_item("characters", counts.0, 0, Some(counts.2)),
                report_item("presets", counts.1, 0, None),
                report_item("conversations", counts.2, 0, Some(counts.2)),
            ],
            converted: Vec::new(),
            excluded: Vec::new(),
        };
        for (code, count) in &projector.losses.0 {
            let item = report_item(code, *count, 0, projector.conversation_scope(code));
            if code.starts_with("converted-") {
                report.converted.push(item)
            } else {
                report.excluded.push(item)
            }
        }
        report.preserved.push(report_item(
            "attachment-files",
            attachments.preserved_files,
            attachments.preserved_bytes,
            None,
        ));
        if target == CompatibilityTarget::PocketRisu {
            let affected: Vec<_> = inventory
                .assets
                .iter()
                .filter(|alias| {
                    alias.kind == "inlay"
                        && alias
                            .metadata
                            .get("pocketRisu")
                            .and_then(|meta| meta.get("charId"))
                            .and_then(Value::as_str)
                            .is_some_and(|id| counts.3.contains(id))
                })
                .collect();
            if !affected.is_empty() {
                report.excluded.push(report_item(
                    "group-provenance-owner-excluded",
                    affected.len() as u64,
                    affected
                        .iter()
                        .map(|alias| u64::try_from(alias.size).unwrap_or(0))
                        .sum(),
                    None,
                ));
            }
        }
        for (code, count, bytes) in &attachments.losses {
            let item = report_item(
                code,
                *count,
                *bytes,
                attachments
                    .affected_conversations
                    .get(code)
                    .copied()
                    .flatten(),
            );
            if code.starts_with("converted-") {
                report.converted.push(item)
            } else {
                report.excluded.push(item)
            }
        }
        let owners = inventory
            .owner_heads
            .iter()
            .filter(|head| head.present)
            .count();
        if owners > 0 {
            report.converted.push(report_item(
                "owner-asset-arrays-rehydrated",
                owners as u64,
                0,
                attachments
                    .affected_conversations
                    .get("owner-asset-arrays-rehydrated")
                    .copied()
                    .flatten(),
            ));
        }
        for code in &attachments.warning_codes {
            if !attachments.losses.iter().any(|item| &item.0 == code) {
                let count = match code.as_str() {
                    "asset-paths-remapped" => attachments.replacements.len(),
                    "inlay-ids-remapped" => attachments.inlay_replacements.len(),
                    _ => 1,
                };
                let item = report_item(
                    code,
                    count as u64,
                    0,
                    attachments
                        .affected_conversations
                        .get(code)
                        .copied()
                        .flatten(),
                );
                if code.contains("unverified") {
                    report.excluded.push(item)
                } else {
                    report.converted.push(item)
                }
            }
        }
        job.set_compatibility_report(report)
            .map_err(job_state_error)?;
        job.set_phase(JobPhase::PublishingDestination)
            .map_err(job_state_error)?;
        let handoff_path = handoff_directory.join(format!("risu-backup-{}.bin", Uuid::new_v4()));
        let output = destination_path.unwrap_or(&handoff_path);
        let output_root = output.parent().ok_or_else(|| {
            NativeJobError::new(
                "invalid-destination",
                "compatibility destination has no parent",
            )
        })?;
        let phase_error = RefCell::new(None);
        let published = destination::write_legacy_backup_destination_controlled(
            owned_directory,
            &archive_path,
            output_root,
            output,
            || job.is_cancel_requested() || phase_error.borrow().is_some(),
            |progress| {
                let _ = job.set_progress(JobProgress {
                    completed_bytes: written.archive_bytes.saturating_add(progress.copied_bytes),
                    total_bytes: Some(written.archive_bytes.saturating_mul(2)),
                    completed_items: written.written_entries,
                    total_items: Some(written.written_entries),
                });
            },
            || {
                job.set_phase(JobPhase::FinalizingExport).map_err(|error| {
                    *phase_error.borrow_mut() = Some(error);
                    destination::DestinationWriteError::Cancelled
                })
            },
        )
        .map_err(|error| destination_job_error(error, phase_error.into_inner()))?;
        Ok(JobResultSummary {
            revision: expected_revision,
            source_bytes: published.bytes,
            source_sha256: published.sha256,
            character_count: counts.0,
            preset_count: counts.1,
            warning_codes: if projector.losses.0.is_empty() && attachments.warning_codes.is_empty()
            {
                vec![]
            } else {
                vec!["compatibility-losses".into()]
            },
            handoff_path: destination_path
                .is_none()
                .then(|| output.to_string_lossy().into_owned()),
            publication: None,
        })
    })();
    let release = prepared.release(reader).map_err(store_job_error);
    let pins = durable
        .as_mut()
        .map(|pins| {
            pins.release(if outcome.is_ok() {
                CasReleaseOutcome::Committed
            } else {
                CasReleaseOutcome::Aborted
            })
            .map_err(io_job_error)
        })
        .unwrap_or(Ok(()));
    match (outcome, release, pins) {
        (Ok(result), Ok(()), Ok(())) => Ok(result),
        (Ok(mut result), _, _) => {
            result.warning_codes.push("cleanup-failed".into());
            Ok(result)
        }
        (Err(error), _, _) => Err(error),
    }
}
fn report_item(code: &str, items: u64, bytes: u64, chats: Option<u64>) -> CompatibilityReportItem {
    CompatibilityReportItem {
        code: code.into(),
        items: items.to_string(),
        bytes: bytes.to_string(),
        affected_conversations: chats.map(|count| count.to_string()),
    }
}
fn sql(error: rusqlite::Error) -> NativeJobError {
    store_job_error(StoreError::from(error))
}
fn parse(text: String) -> Result<Value, NativeJobError> {
    serde_json::from_str(&text).map_err(|_| {
        NativeJobError::new(
            "invalid-source",
            "invalid JSON record in compatibility snapshot",
        )
    })
}
fn object(value: &mut Value) -> Result<&mut Map<String, Value>, NativeJobError> {
    value.as_object_mut().ok_or_else(|| {
        NativeJobError::new("invalid-source", "compatibility record is not an object")
    })
}
fn count(
    connection: &Connection,
    query: &str,
    values: &[&dyn rusqlite::ToSql],
) -> Result<u64, NativeJobError> {
    let n: i64 = connection
        .query_row(query, values, |row| row.get(0))
        .map_err(sql)?;
    u64::try_from(n).map_err(|_| NativeJobError::new("invalid-source", "invalid snapshot count"))
}

fn write_database(
    connection: &Connection,
    read: &ReadTarget,
    owner: &OwnerManifestProjector,
    projector: &mut Projector,
    assets: &HashMap<String, String>,
    inlays: &HashMap<String, String>,
    path: &Path,
    cancel: &dyn CancellationProbe,
) -> Result<(u64, u64, u64, HashSet<String>), NativeJobError> {
    let generation = &read.generation;
    let orphans=count(connection,"SELECT count(*) FROM conversations c LEFT JOIN characters p ON p.generation=c.generation AND p.character_id=c.character_id WHERE c.generation=?1 AND p.character_id IS NULL",&[generation])?
        +count(connection,"SELECT count(*) FROM messages m LEFT JOIN conversations c ON c.generation=m.generation AND c.character_id=m.character_id AND c.conversation_id=m.conversation_id WHERE m.generation=?1 AND c.conversation_id IS NULL",&[generation])?;
    if orphans > 0 {
        return Err(NativeJobError::new("invalid-source","compatibility snapshot contains orphan conversation or message rows; use a native backup to preserve repair data"));
    }
    // COUNT and cursor share one leased, immutable snapshot. Parse all IDs before
    // writing array headers so Pocket group removal cannot disagree with count.
    let mut ids = Vec::new();
    let mut groups = HashSet::new();
    let mut statement=connection.prepare("SELECT character_id,detail FROM characters WHERE generation=?1 ORDER BY configured_index ASC").map_err(sql)?;
    let mut rows = statement.query([generation]).map_err(sql)?;
    while let Some(row) = rows.next().map_err(sql)? {
        check_cancelled(cancel).map_err(local_backup_error)?;
        let id: String = row.get(0).map_err(sql)?;
        let value = parse(row.get(1).map_err(sql)?)?;
        if projector.target == CompatibilityTarget::PocketRisu && value["type"] == "group" {
            groups.insert(id);
        } else {
            ids.push(id);
        }
    }
    owner
        .validate_character_owners(&ids.iter().cloned().chain(groups.iter().cloned()).collect())
        .map_err(store_job_error)?;
    projector
        .losses
        .add("unsupported-groups", groups.len() as u64);
    let mut excluded_group_chats = 0;
    for group in &groups {
        excluded_group_chats += count(
            connection,
            "SELECT count(*) FROM conversations WHERE generation=?1 AND character_id=?2",
            &[generation, group],
        )?;
    }
    if !groups.is_empty() {
        projector.known_conversation_scope("unsupported-groups", excluded_group_chats);
    }
    let preset_count = count(
        connection,
        "SELECT count(*) FROM bot_presets WHERE generation=?1",
        &[generation],
    )?;
    let mut root = parse(
        connection
            .query_row(
                "SELECT value FROM root WHERE generation=?1",
                [generation],
                |r| r.get(0),
            )
            .map_err(sql)?,
    )?;
    owner
        .project_root(object(&mut root)?)
        .map_err(store_job_error)?;
    for key in [
        "characters",
        "botPresets",
        "pluginCustomStorage",
        "account",
        "__directory",
    ] {
        object(&mut root)?.remove(key);
    }
    clean_group_order(&mut root, &groups);
    rewrite_assets(&mut root, assets);
    let root = projector.project("DataBase", &root)?;
    let mut file = File::create(path).map_err(io_job_error)?;
    file.write_all(COMPRESSED_MAGIC).map_err(io_job_error)?;
    let mut writer = GzEncoder::new(file, Compression::default());
    map_header(&mut writer, root.as_object().unwrap().len() as u64 + 3)?;
    write_pairs(&mut writer, &root)?;
    string(&mut writer, "characters")?;
    array_header(&mut writer, ids.len() as u64)?;
    let mut conversations = 0;
    for id in &ids {
        check_cancelled(cancel).map_err(local_backup_error)?;
        let mut character = parse(
            connection
                .query_row(
                    "SELECT detail FROM characters WHERE generation=?1 AND character_id=?2",
                    params![generation, id],
                    |r| r.get(0),
                )
                .map_err(sql)?,
        )?;
        owner
            .project_character(id, object(&mut character)?)
            .map_err(store_job_error)?;
        object(&mut character)?.remove("chats");
        rewrite_assets(&mut character, assets);
        rewrite_references(&mut character, inlays, false);
        let kind = if character["type"] == "group" {
            "groupChat"
        } else {
            "character"
        };
        let character = projector.project(kind, &character)?;
        let chats = count(
            connection,
            "SELECT count(*) FROM conversations WHERE generation=?1 AND character_id=?2",
            &[generation, id],
        )?;
        map_header(&mut writer, character.as_object().unwrap().len() as u64 + 1)?;
        write_pairs(&mut writer, &character)?;
        string(&mut writer, "chats")?;
        array_header(&mut writer, chats)?;
        let mut statement=connection.prepare("SELECT conversation_id,detail FROM conversations WHERE generation=?1 AND character_id=?2 ORDER BY configured_index ASC").map_err(sql)?;
        let mut rows = statement.query(params![generation, id]).map_err(sql)?;
        let mut actual = 0;
        while let Some(row) = rows.next().map_err(sql)? {
            check_cancelled(cancel).map_err(local_backup_error)?;
            let before = projector.losses.0.clone();
            let chat_id: String = row.get(0).map_err(sql)?;
            let mut chat = parse(row.get(1).map_err(sql)?)?;
            let recovery = object(&mut chat)?.remove("rerollRecovery");
            object(&mut chat)?.remove("message");
            rewrite_references(&mut chat, inlays, false);
            let chat = projector.project("Chat", &chat)?;
            map_header(&mut writer, chat.as_object().unwrap().len() as u64 + 1)?;
            write_pairs(&mut writer, &chat)?;
            string(&mut writer, "message")?;
            write_messages(
                connection,
                generation,
                id,
                &chat_id,
                recovery.as_ref(),
                projector,
                inlays,
                &mut writer,
                cancel,
            )?;
            projector.record_conversation(&before);
            actual += 1;
        }
        if actual != chats {
            return Err(NativeJobError::new(
                "invalid-source",
                "conversation cursor count changed",
            ));
        }
        conversations += actual;
    }
    string(&mut writer, "botPresets")?;
    array_header(&mut writer, preset_count)?;
    let mut statement = connection
        .prepare("SELECT value FROM bot_presets WHERE generation=?1 ORDER BY configured_index ASC")
        .map_err(sql)?;
    let mut rows = statement.query([generation]).map_err(sql)?;
    let mut actual = 0;
    while let Some(row) = rows.next().map_err(sql)? {
        check_cancelled(cancel).map_err(local_backup_error)?;
        let mut value = parse(row.get(0).map_err(sql)?)?;
        rewrite_assets(&mut value, assets);
        write_value(&mut writer, &projector.project("botPreset", &value)?)?;
        actual += 1;
    }
    if actual != preset_count {
        return Err(NativeJobError::new(
            "invalid-source",
            "preset cursor count changed",
        ));
    }
    string(&mut writer, "pluginCustomStorage")?;
    // A legacy entry holds one value per key, so a key two plugins both hold
    // cannot go out without handing one plugin the other's value.
    let storage_count = count(
        connection,
        "SELECT count(*) FROM plugin_storage WHERE generation=?1 AND storage_key NOT IN (SELECT storage_key FROM plugin_storage WHERE generation=?1 GROUP BY storage_key HAVING count(*)>1)",
        &[generation],
    )?;
    map_header(&mut writer, storage_count)?;
    let mut statement=connection.prepare("SELECT storage_key,value FROM plugin_storage WHERE generation=?1 AND storage_key NOT IN (SELECT storage_key FROM plugin_storage WHERE generation=?1 GROUP BY storage_key HAVING count(*)>1) ORDER BY CASE WHEN storage_key NOT GLOB '*[^0-9]*' AND storage_key != '' AND CAST(CAST(storage_key AS INTEGER) AS TEXT)=storage_key AND CAST(storage_key AS INTEGER)<4294967295 THEN 0 ELSE 1 END, CASE WHEN storage_key NOT GLOB '*[^0-9]*' AND storage_key != '' AND CAST(CAST(storage_key AS INTEGER) AS TEXT)=storage_key AND CAST(storage_key AS INTEGER)<4294967295 THEN CAST(storage_key AS INTEGER) END, ordinal ASC").map_err(sql)?;
    let mut rows = statement.query([generation]).map_err(sql)?;
    let mut actual = 0;
    while let Some(row) = rows.next().map_err(sql)? {
        check_cancelled(cancel).map_err(local_backup_error)?;
        let key: String = row.get(0).map_err(sql)?;
        let mut value = parse(row.get(1).map_err(sql)?)?;
        rewrite_plugin(&mut value, assets, &mut projector.losses);
        string(&mut writer, &key)?;
        write_value(&mut writer, &value)?;
        actual += 1;
    }
    if actual != storage_count {
        return Err(NativeJobError::new(
            "invalid-source",
            "plugin cursor count changed",
        ));
    }
    let file = writer.finish().map_err(io_job_error)?;
    file.sync_all().map_err(io_job_error)?;
    if file.metadata().map_err(io_job_error)?.len() > u32::MAX as u64 {
        return Err(NativeJobError::new(
            "unsupported-format",
            "target .bin database entry exceeds 4 GiB limit; use a RisuNest backup",
        ));
    }
    Ok((ids.len() as u64, preset_count, conversations, groups))
}

fn clean_group_order(root: &mut Value, groups: &HashSet<String>) {
    if let Some(order) = root.get_mut("characterOrder").and_then(Value::as_array_mut) {
        order.retain(|item| !item.as_str().is_some_and(|id| groups.contains(id)));
        for item in order {
            if let Some(children) = item.get_mut("data").and_then(Value::as_array_mut) {
                children.retain(|child| !child.as_str().is_some_and(|id| groups.contains(id)));
            }
        }
    }
}

fn each_message(
    connection: &Connection,
    generation: &str,
    character: &str,
    chat: &str,
    mut visit: impl FnMut(usize, Value) -> Result<(), NativeJobError>,
) -> Result<usize, NativeJobError> {
    let mut statement=connection.prepare("SELECT value FROM messages WHERE generation=?1 AND character_id=?2 AND conversation_id=?3 ORDER BY message_index ASC").map_err(sql)?;
    let mut rows = statement
        .query(params![generation, character, chat])
        .map_err(sql)?;
    let mut index = 0;
    while let Some(row) = rows.next().map_err(sql)? {
        visit(index, parse(row.get(0).map_err(sql)?)?)?;
        index += 1;
    }
    Ok(index)
}
fn write_messages(
    connection: &Connection,
    generation: &str,
    character: &str,
    chat: &str,
    recovery: Option<&Value>,
    projector: &mut Projector,
    inlays: &HashMap<String, String>,
    writer: &mut dyn Write,
    cancel: &dyn CancellationProbe,
) -> Result<(), NativeJobError> {
    let recovery = recovery.filter(|r| r["phase"] == "generating");
    if recovery.is_none() {
        let total=count(connection,"SELECT count(*) FROM messages WHERE generation=?1 AND character_id=?2 AND conversation_id=?3",&[&generation,&character,&chat])?;
        array_header(writer, total)?;
        let actual = each_message(connection, generation, character, chat, |_, mut value| {
            check_cancelled(cancel).map_err(local_backup_error)?;
            if projector.target == CompatibilityTarget::PocketRisu {
                pocket_swipes(&mut value, &mut projector.losses)
            } else if value.get("responseVariants").is_some() {
                projector.losses.add("unsupported-reroll-candidates", 1)
            }
            rewrite_references(&mut value, inlays, true);
            write_value(writer, &projector.project("Message", &value)?)
        })?;
        if actual as u64 != total {
            return Err(NativeJobError::new(
                "invalid-source",
                "message cursor count changed",
            ));
        }
        return Ok(());
    }
    let mut anchor = None;
    let length = each_message(connection, generation, character, chat, |index, value| {
        check_cancelled(cancel).map_err(local_backup_error)?;
        if anchor.is_none()
            && recovery.is_some_and(|r| {
                r["anchorId"].as_str().is_some_and(|id| !id.is_empty())
                    && r["anchorId"] == value["chatId"]
            })
        {
            anchor = Some(index)
        }
        Ok(())
    })?;
    let start = recovery
        .map(|r| {
            anchor
                .map(|n| n + 1)
                .unwrap_or_else(|| r["startIndex"].as_u64().unwrap_or(0) as usize)
                .min(length)
        })
        .unwrap_or(length);
    let original = recovery.and_then(|r| r["original"].as_array());
    if recovery.is_some() && original.is_none() {
        return Err(NativeJobError::new(
            "invalid-source",
            "reroll recovery original is invalid",
        ));
    }
    let keep = |index: usize, value: &Value| {
        if index < start {
            return true;
        }
        let owned =
            recovery.and_then(|r| value["chatId"].as_str().and_then(|id| r["outputs"].get(id)));
        !owned.is_some_and(|owned| {
            serde_json::to_string(owned).ok() == serde_json::to_string(value).ok()
        })
    };
    let mut count = original.map(Vec::len).unwrap_or(0);
    each_message(connection, generation, character, chat, |index, value| {
        if keep(index, &value) {
            count += 1
        }
        Ok(())
    })?;
    array_header(writer, count as u64)?;
    let mut written = 0;
    let mut write_one = |mut value: Value| -> Result<(), NativeJobError> {
        check_cancelled(cancel).map_err(local_backup_error)?;
        if projector.target == CompatibilityTarget::PocketRisu {
            pocket_swipes(&mut value, &mut projector.losses)
        } else if value.get("responseVariants").is_some() {
            projector.losses.add("unsupported-reroll-candidates", 1)
        }
        rewrite_references(&mut value, inlays, true);
        write_value(writer, &projector.project("Message", &value)?)?;
        written += 1;
        Ok(())
    };
    each_message(connection, generation, character, chat, |index, value| {
        if index == start {
            if let Some(original) = original {
                for value in original {
                    write_one(value.clone())?
                }
            }
        }
        if keep(index, &value) {
            write_one(value)?
        }
        Ok(())
    })?;
    if start == length {
        if let Some(original) = original {
            for value in original {
                write_one(value.clone())?
            }
        }
    }
    if written != count {
        return Err(NativeJobError::new(
            "invalid-source",
            "message cursor count changed",
        ));
    }
    if recovery.is_some() {
        projector.losses.add("converted-interrupted-reroll", 1)
    }
    Ok(())
}

fn rewrite_references(value: &mut Value, inlays: &HashMap<String, String>, message: bool) {
    if let Some(object) = value.as_object_mut() {
        if message {
            for key in ["data", "swipes"] {
                if let Some(value) = object.get_mut(key) {
                    rewrite_message_text(value, inlays)
                }
            }
        }
    }
}
fn rewrite_message_text(value: &mut Value, inlays: &HashMap<String, String>) {
    match value {
        Value::String(text) => {
            // Inlay grammar is explicit. Never replace bare IDs or arbitrary substrings.
            for (old, new) in inlays {
                for kind in ["inlay", "inlayed", "inlayeddata"] {
                    *text = text.replace(
                        &format!("{{{{{kind}::{old}}}}}"),
                        &format!("{{{{{kind}::{new}}}}}"),
                    );
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                rewrite_message_text(item, inlays)
            }
        }
        _ => {}
    }
}
fn map_header(writer: &mut dyn Write, len: u64) -> Result<(), NativeJobError> {
    length_header(writer, len, 0xdf)
}
fn array_header(writer: &mut dyn Write, len: u64) -> Result<(), NativeJobError> {
    length_header(writer, len, 0xdd)
}
fn length_header(writer: &mut dyn Write, len: u64, tag: u8) -> Result<(), NativeJobError> {
    let length = u32::try_from(len).map_err(|_| {
        NativeJobError::new(
            "unsupported-format",
            "target MessagePack collection exceeds 32-bit limit",
        )
    })?;
    writer.write_all(&[tag]).map_err(io_job_error)?;
    writer
        .write_all(&length.to_be_bytes())
        .map_err(io_job_error)
}
fn string(writer: &mut dyn Write, text: &str) -> Result<(), NativeJobError> {
    length_header(writer, text.len() as u64, 0xdb)?;
    writer.write_all(text.as_bytes()).map_err(io_job_error)
}
fn write_pairs(writer: &mut dyn Write, value: &Value) -> Result<(), NativeJobError> {
    for (key, value) in value
        .as_object()
        .ok_or_else(|| NativeJobError::new("invalid-source", "expected map"))?
    {
        string(writer, key)?;
        write_value(writer, value)?
    }
    Ok(())
}
fn write_value(writer: &mut dyn Write, value: &Value) -> Result<(), NativeJobError> {
    match value {
        Value::Null => writer.write_all(&[0xc0]).map_err(io_job_error),
        Value::Bool(b) => writer
            .write_all(&[if *b { 0xc3 } else { 0xc2 }])
            .map_err(io_job_error),
        Value::String(s) => string(writer, s),
        Value::Number(number) => {
            // msgpackr decodes 64-bit integer tags as BigInt by default. JS JSON
            // numbers use 32-bit integer tags or float64, never those tags.
            if let Some(n) = number.as_u64().and_then(|n| u32::try_from(n).ok()) {
                writer.write_all(&[0xce]).map_err(io_job_error)?;
                return writer.write_all(&n.to_be_bytes()).map_err(io_job_error);
            }
            if let Some(n) = number.as_i64().and_then(|n| i32::try_from(n).ok()) {
                writer.write_all(&[0xd2]).map_err(io_job_error)?;
                return writer.write_all(&n.to_be_bytes()).map_err(io_job_error);
            }
            let n = number.as_f64().ok_or_else(|| {
                NativeJobError::new("invalid-source", "unsupported target number")
            })?;
            if number.as_u64().is_some_and(|v| n as u128 != v as u128)
                || number.as_i64().is_some_and(|v| n as i128 != v as i128)
            {
                return Err(NativeJobError::new(
                    "unsupported-projection",
                    "integer cannot be represented exactly by target JavaScript number",
                ));
            }
            writer.write_all(&[0xcb]).map_err(io_job_error)?;
            writer.write_all(&n.to_be_bytes()).map_err(io_job_error)
        }
        Value::Array(values) => {
            array_header(writer, values.len() as u64)?;
            for value in values {
                write_value(writer, value)?
            }
            Ok(())
        }
        Value::Object(values) => {
            map_header(writer, values.len() as u64)?;
            write_pairs(writer, value)
        }
    }
}
/// A streaming structural decoder verifies every declared count and gzip trailer
/// without retaining the generated database. Reference-reader tests verify values.
fn validate_database(path: &Path, cancel: &dyn CancellationProbe) -> Result<(), NativeJobError> {
    let mut source = File::open(path).map_err(io_job_error)?;
    let mut magic = [0; 11];
    source.read_exact(&mut magic).map_err(io_job_error)?;
    if magic != COMPRESSED_MAGIC {
        return Err(NativeJobError::new(
            "invalid-source",
            "invalid target DB magic",
        ));
    }
    let mut reader = GzDecoder::new(CancellationReader::new(source, cancel));
    scan_value(&mut reader, 0)?;
    let mut extra = [0];
    if reader.read(&mut extra).map_err(io_job_error)? != 0 {
        return Err(NativeJobError::new(
            "invalid-source",
            "trailing target database data",
        ));
    }
    Ok(())
}
fn scan_value(reader: &mut dyn Read, depth: usize) -> Result<(), NativeJobError> {
    if depth > 256 {
        return Err(NativeJobError::new(
            "invalid-source",
            "target database nesting limit",
        ));
    }
    let mut tag = [0];
    reader.read_exact(&mut tag).map_err(io_job_error)?;
    match tag[0] {
        0xc0 | 0xc2 | 0xc3 => Ok(()),
        0xce | 0xd2 => {
            let mut bytes = [0; 4];
            reader.read_exact(&mut bytes).map_err(io_job_error)
        }
        0xcf | 0xd3 | 0xcb => {
            let mut bytes = [0; 8];
            reader.read_exact(&mut bytes).map_err(io_job_error)
        }
        0xdb | 0xdd | 0xdf => {
            let mut bytes = [0; 4];
            reader.read_exact(&mut bytes).map_err(io_job_error)?;
            let n = u32::from_be_bytes(bytes) as u64;
            if tag[0] == 0xdb {
                let read = io::copy(&mut reader.take(n), &mut io::sink()).map_err(io_job_error)?;
                if read != n {
                    return Err(NativeJobError::new(
                        "invalid-source",
                        "truncated target string",
                    ));
                }
                Ok(())
            } else {
                for _ in 0..n * if tag[0] == 0xdf { 2 } else { 1 } {
                    scan_value(reader, depth + 1)?
                }
                Ok(())
            }
        }
        _ => Err(NativeJobError::new(
            "invalid-source",
            "unexpected MessagePack tag in target DB",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_file_jobs::{JobKind, JobRegistry};
    use serde_json::json;

    fn decode(path: &Path) -> Value {
        let bytes = std::fs::read(path).unwrap();
        let mut offset = 0;
        let mut database = None;
        let mut names = HashSet::new();
        while offset < bytes.len() {
            let length = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
            offset += 4;
            let name = std::str::from_utf8(&bytes[offset..offset + length]).unwrap();
            offset += length;
            let length = u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
            offset += 4;
            assert!(names.insert(name.to_owned()));
            if name == DATABASE_ENTRY {
                assert_eq!(offset + length, bytes.len());
                database = Some(bytes[offset..offset + length].to_vec());
            }
            offset += length;
        }
        let database = database.unwrap();
        assert_eq!(&database[..11], COMPRESSED_MAGIC);
        let mut reader = GzDecoder::new(&database[11..]);
        let value = rmpv::decode::read_value(&mut reader).unwrap();
        fn json_value(value: rmpv::Value) -> Value {
            match value {
                rmpv::Value::Nil => Value::Null,
                rmpv::Value::Boolean(v) => Value::Bool(v),
                rmpv::Value::Integer(v) => {
                    if let Some(n) = v.as_u64() {
                        json!(n)
                    } else {
                        json!(v.as_i64().unwrap())
                    }
                }
                rmpv::Value::F64(v) => json!(v),
                rmpv::Value::F32(v) => json!(v),
                rmpv::Value::String(v) => json!(v.as_str().unwrap()),
                rmpv::Value::Array(v) => Value::Array(v.into_iter().map(json_value).collect()),
                rmpv::Value::Map(v) => Value::Object(
                    v.into_iter()
                        .map(|(k, v)| (k.as_str().unwrap().to_owned(), json_value(v)))
                        .collect(),
                ),
                _ => panic!("unsupported msgpack value"),
            }
        }
        json_value(value)
    }

    #[test]
    fn compatible_export_streams_authoritative_rows_and_strips_target_only_fields() {
        for target in [CompatibilityTarget::RisuAi, CompatibilityTarget::PocketRisu] {
            let directory = tempfile::tempdir().unwrap();
            let mut store = PersistentStore::open(directory.path()).unwrap();
            let staging = store.replace_begin().unwrap().staging_id;
            let cas = PayloadCas::new(directory.path()).unwrap();
            let png=hex::decode("89504e470d0a1a0a0000000d49484452000000010000000108060000001f15c4890000000b49444154789c636000020000050001a5f645400000000049454e44ae426082").unwrap();
            let payload = cas.prepare_bytes(&png).unwrap();
            let owner_manifest =
                crate::asset_repository::owner_manifest_codec::encode_owner_manifest(&[
                    crate::asset_repository::owner_manifest_codec::OwnerManifestEntry {
                        tuple: [
                            "Synthetic owner image".into(),
                            "assets/prior-owner.png".into(),
                            "png".into(),
                        ],
                        payload_hash: Some(
                            hex::decode(&payload.content_hash)
                                .unwrap()
                                .try_into()
                                .unwrap(),
                        ),
                    },
                ])
                .unwrap();
            let owner_manifest = cas.prepare_bytes(&owner_manifest).unwrap();
            store
                .replace_put_asset_aliases(
                    &staging,
                    &[
                        AssetAlias {
                            key: "synthetic".into(),
                            object_hash: Some(payload.content_hash.clone()),
                            kind: "inlay".into(),
                            size: payload.byte_size as i64,
                            mime: "image/png".into(),
                            name: "synthetic.png".into(),
                            ext: "png".into(),
                            inlay_type: Some("image".into()),
                            width: Some(1),
                            height: Some(1),
                            metadata: json!({}),
                        },
                        AssetAlias {
                            key: "assets/nested/icon.webp".into(),
                            object_hash: Some(payload.content_hash),
                            kind: "asset".into(),
                            size: payload.byte_size as i64,
                            mime: "image/png".into(),
                            name: "icon.webp".into(),
                            ext: "webp".into(),
                            inlay_type: None,
                            width: None,
                            height: None,
                            metadata: json!({}),
                        },
                    ],
                )
                .unwrap();
            let mut root = json!({"account":{"token":"synthetic-token"},"characterOrder":["synthetic",{"name":"Folder","data":["group","synthetic"],"id":"folder","color":"red","img":"assets/nested/icon.webp"},"group"],"plugins":[],"modules":[],"personas":[],"loadouts":[],"disableToggleBinding":true,"defaultToggleValues":{"test":"1"},"risunestInlayMode":"unsupported","streamingThoughtMode":"unsupported","unknownRoot":true});
            root["pluginCustomStorage"] = json!({"10":null,"2":true,"01":"leading zero","z":{"exact":"assets/nested/icon.webp","opaque":"prefix assets/nested/icon.webp suffix","large":"큰".repeat(65536)},"4294967295":9007199254740991u64});
            store.replace_put_root(&staging, &root).unwrap();
            store
                .replace_put_presets(
                    &staging,
                    &[json!({"name":"Synthetic Preset","image":"assets/nested/icon.webp"})],
                )
                .unwrap();
            let message = json!({"role":"char","data":"현재 한국어 {{inlay::synthetic}}","chatId":"message","unknownMessage":true,"responseVariants":{"groupId":"message","selectedId":"selected","candidates":[{"id":"other","messages":[{"role":"char","data":"alternative"}]},{"id":"complex","messages":[{"role":"char","data":"one"},{"role":"char","data":"two"}]},{"id":"selected","messages":[{"role":"char","data":"stale selected"}]}]}});
            store.replace_add_characters(&staging,&[json!({"chaId":"synthetic","type":"character","name":"Synthetic","additionalAssets":[["Synthetic owner image","assets/prior-owner.png","png"]],"chats":[{"id":"chat","name":"Chat","message":[message,{"role":"user","data":"followup"}],"savedToggleValues":{"test":"1"},"unknownChat":true}]}),json!({"chaId":"group","type":"group","name":"Synthetic Group","characters":["synthetic"],"chats":[]})]).unwrap();
            store
                .replace_put_asset_owner_heads(
                    &staging,
                    &[AssetOwnerHead::present(
                        AssetOwnerLocator::CharacterAdditionalAssets {
                            character_id: "synthetic".into(),
                        },
                        owner_manifest.content_hash,
                        1,
                    )],
                )
                .unwrap();
            store
                .replace_put_asset_repository_authority(
                    &staging,
                    &AssetRepositoryAuthorityState::V2 {
                        migration_id: "synthetic-assets".into(),
                        compatibility_hash: "ab".repeat(32),
                    },
                )
                .unwrap();
            let revision = store.replace_commit(&staging, Some(0)).unwrap().revision;
            // A key two plugins both hold cannot go out without handing one of
            // them the other's value, so neither side is written.
            let revision = store
                .commit(&crate::persistent_store::WorkingSetCommit {
                    expected_revision: revision,
                    root: None,
                    root_mutations: None,
                    replace_presets: None,
                    character: None,
                    character_details: None,
                    replace_character: None,
                    add_character: None,
                    conversations: None,
                    delete_character_id: None,
                    plugin_storage: Some(vec![
                        crate::persistent_store::PluginStorageMutation::Set {
                            owner: "plugin-a".to_owned(),
                            key: "shared_key".to_owned(),
                            value: json!("a value"),
                        },
                        crate::persistent_store::PluginStorageMutation::Set {
                            owner: crate::persistent_store::plugin_owner::UNOWNED_OWNER
                                .to_owned(),
                            key: "shared_key".to_owned(),
                            value: json!("imported value"),
                        },
                        crate::persistent_store::PluginStorageMutation::Set {
                            owner: "plugin-a".to_owned(),
                            key: "owned_key".to_owned(),
                            value: json!("kept"),
                        },
                    ]),
                    asset_owner_heads: None,
                })
                .unwrap()
                .revision;
            let owned = directory.path().join("owned");
            let handoff = directory.path().join("handoff");
            std::fs::create_dir(&owned).unwrap();
            std::fs::create_dir(&handoff).unwrap();
            let output = directory.path().join("export.bin");
            let job = JobRegistry::default()
                .create(JobKind::ExportLegacyLocalBackup)
                .unwrap();
            export_compatible_local_backup(
                target,
                Some(&output),
                revision,
                &owned,
                &handoff,
                store,
                &job,
            )
            .unwrap();
            let report = job.status().compatibility_report.unwrap();
            assert_eq!(report.target, target);
            let code = if target == CompatibilityTarget::PocketRisu {
                "converted-swipes"
            } else {
                "unsupported-inlay-references"
            };
            let scoped = report
                .converted
                .iter()
                .chain(&report.excluded)
                .find(|item| item.code == code)
                .unwrap();
            assert_eq!(scoped.affected_conversations.as_deref(), Some("1"));
            if target == CompatibilityTarget::PocketRisu {
                let group = report
                    .excluded
                    .iter()
                    .find(|item| item.code == "unsupported-groups")
                    .unwrap();
                assert_eq!(group.affected_conversations.as_deref(), Some("0"));
            }
            let db = decode(&output);
            assert_eq!(db["botPresets"][0]["image"], db["characterOrder"][1]["img"]);
            let owner_assets = db["characters"][0]["additionalAssets"].as_array().unwrap();
            assert_eq!(owner_assets.len(), 1);
            assert_eq!(owner_assets[0][0], "Synthetic owner image");
            assert_eq!(owner_assets[0][2], "png");
            assert_ne!(owner_assets[0][1], "assets/prior-owner.png");
            assert_ne!(db["botPresets"][0]["image"], "assets/nested/icon.webp");
            assert_eq!(
                db["pluginCustomStorage"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                vec!["2", "10", "01", "z", "4294967295", "owned_key"]
            );
            assert!(db["pluginCustomStorage"].get("shared_key").is_none());
            assert_eq!(
                db["pluginCustomStorage"]["z"]["exact"],
                db["botPresets"][0]["image"]
            );
            assert_eq!(
                db["pluginCustomStorage"]["z"]["opaque"],
                "prefix assets/nested/icon.webp suffix"
            );
            assert_eq!(db["pluginCustomStorage"]["z"]["large"], "큰".repeat(65536));
            assert!(db.get("account").is_none());
            assert!(db.get("unknownRoot").is_none());
            assert!(db.get("risunestInlayMode").is_none());
            let chat = &db["characters"][0]["chats"][0];
            assert_eq!(chat["message"].as_array().unwrap().len(), 2);
            assert!(chat.get("unknownChat").is_none());
            let message = &chat["message"][0];
            assert_eq!(message["data"], "현재 한국어 {{inlay::synthetic}}");
            assert!(message.get("responseVariants").is_none());
            assert!(message.get("unknownMessage").is_none());
            if target == CompatibilityTarget::PocketRisu {
                assert_eq!(db["characters"].as_array().unwrap().len(), 1);
                assert_eq!(db["characterOrder"][1]["data"], json!(["synthetic"]));
                assert_eq!(
                    message["swipes"],
                    json!(["alternative", "현재 한국어 {{inlay::synthetic}}"])
                );
                assert_eq!(message["swipeId"], 1);
                assert_eq!(chat["savedToggleValues"], json!({"test":"1"}));
            } else {
                assert_eq!(db["characters"].as_array().unwrap().len(), 2);
                assert!(message.get("swipes").is_none());
                assert!(chat.get("savedToggleValues").is_none());
                assert!(db.get("disableToggleBinding").is_none());
            }
            if let Some(dir) = std::env::var_os("RISUNEST_COMPAT_FIXTURE_DIR") {
                let dir = Path::new(&dir);
                std::fs::create_dir_all(dir).unwrap();
                std::fs::copy(
                    &output,
                    dir.join(if target == CompatibilityTarget::RisuAi {
                        "risuai.bin"
                    } else {
                        "pocket.bin"
                    }),
                )
                .unwrap();
            }
        }
    }

    #[test]
    fn compatible_messagepack_uses_js_number_tags_and_checks_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let file = File::create(&path).unwrap();
        let mut encoder = GzEncoder::new(file, Compression::default());
        let value = json!([
            null,
            true,
            false,
            "한국어",
            0,
            4294967295u64,
            9007199254740991u64,
            -2147483649i64,
            1.25
        ]);
        write_value(&mut encoder, &value).unwrap();
        let file = encoder.finish().unwrap();
        file.sync_all().unwrap();
        let mut decoded = Vec::new();
        GzDecoder::new(File::open(&path).unwrap())
            .read_to_end(&mut decoded)
            .unwrap();
        assert!(!decoded.contains(&0xcf));
        assert!(!decoded.contains(&0xd3));
        assert!(write_value(&mut Vec::new(), &json!(9007199254740993u64)).is_err());
        assert!(scan_value(&mut &decoded[..decoded.len() - 1], 0).is_err());
    }

    #[test]
    fn compatible_report_preserves_exact_counts_and_distinguishes_unknown_scope() {
        let unknown =
            serde_json::to_value(report_item("synthetic", u64::MAX, u64::MAX, None)).unwrap();
        assert_eq!(unknown["items"], u64::MAX.to_string());
        assert_eq!(unknown["bytes"], u64::MAX.to_string());
        assert!(unknown["affectedConversations"].is_null());
        let none = serde_json::to_value(report_item("synthetic", 1, 0, Some(0))).unwrap();
        assert_eq!(none["affectedConversations"], "0");
    }

    #[test]
    fn compatible_interrupted_reroll_restores_original_and_retains_independent_edits() {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch("CREATE TABLE messages(generation TEXT,character_id TEXT,conversation_id TEXT,message_index INTEGER,value TEXT)").unwrap();
        let messages = [
            json!({"role":"user","data":"anchor","chatId":"anchor"}),
            json!({"role":"char","data":"owned partial","chatId":"owned"}),
            json!({"role":"char","data":"plugin edit","chatId":"edited"}),
            json!({"role":"user","data":"followup","chatId":"anchor"}),
        ];
        for (index, message) in messages.iter().enumerate() {
            connection
                .execute(
                    "INSERT INTO messages VALUES('g','c','h',?1,?2)",
                    params![index as i64, serde_json::to_string(message).unwrap()],
                )
                .unwrap();
        }
        let recovery = json!({"phase":"generating","startIndex":1,"anchorId":"anchor","original":[{"role":"char","data":"original","chatId":"original"}],"outputs":{"owned":messages[1],"edited":{"role":"char","data":"prior output","chatId":"edited"}}});
        let mut projector = Projector::new(CompatibilityTarget::PocketRisu).unwrap();
        let mut bytes = Vec::new();
        write_messages(
            &connection,
            "g",
            "c",
            "h",
            Some(&recovery),
            &mut projector,
            &HashMap::new(),
            &mut bytes,
            &crate::local_backup::NeverCancelled,
        )
        .unwrap();
        let value = rmpv::decode::read_value(&mut bytes.as_slice()).unwrap();
        let texts: Vec<_> = value
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                m.as_map()
                    .unwrap()
                    .iter()
                    .find(|(k, _)| k.as_str() == Some("data"))
                    .unwrap()
                    .1
                    .as_str()
                    .unwrap()
            })
            .collect();
        assert_eq!(texts, vec!["anchor", "original", "plugin edit", "followup"]);
        assert_eq!(projector.losses.0["converted-interrupted-reroll"], 1);
    }
}
