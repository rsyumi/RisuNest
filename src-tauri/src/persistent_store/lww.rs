use super::{
    active_generation, commit, current_revision, device_store, PersistentStore, RevisionResult,
    StoreError, StoreResult, WorkingSetCommit,
};
use risunest_sync_wire::{
    descriptor::RecordDescriptor,
    stamp::{issue_stamp, DecimalU64, Stamp},
    unit::{compare_version, LwwDecision, UnitKey, UnitValue, MAX_INLINE_UNIT_BYTES},
};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
};
use uuid::Uuid;

#[path = "lww_classification.rs"]
mod classification;
#[path = "lww_quarantine.rs"]
mod quarantine;
#[path = "lww_binding_stage.rs"]
mod binding_stage;
pub(crate) use binding_stage::{BindingUnitStage, StagingObserver, StagingProgress};
pub(super) use binding_stage::{binding_source_rows, source_digest_rows, catalog_digest, catalog_digest_observed, seed_binding_holds, validate_binding_source, validate_binding_source_rows, BINDING_STAGE_SCHEMA};
#[path = "lww_intent_rows.rs"]
pub(super) mod intent_rows;
pub(super) use intent_rows::FrozenRows;
#[path = "lww_new_device.rs"]
mod new_device;
#[path = "lww_projection.rs"]
mod projection;
#[path = "lww_replacement_source.rs"]
mod replacement_source;
pub(super) use replacement_source::{SourceLayer, REMAPPED as REMAPPED_SOURCE, STAGED as STAGED_SOURCE};
pub(super) use replacement_source::{clear as clear_replacement_source, insert as insert_replacement_source, units as replacement_source_units};
pub(super) use projection::validate_received;
pub(super) use projection::{
    apply_mutations, capture_device_changes, capture_targets, is_device, preserve_local_root,
    refresh_orders, archive_object_hashes, archive_value,
};

pub(crate) fn lww_known_unit_key(key: &UnitKey) -> bool {
    projection::known(key)
}

pub(super) const UNIT_SCHEMA: &str = r#"CREATE TABLE lww_units(key TEXT PRIMARY KEY,stamp TEXT NOT NULL,value TEXT NOT NULL,version TEXT NOT NULL,identity TEXT NOT NULL);
CREATE TABLE lww_outbox(key TEXT PRIMARY KEY,stamp TEXT NOT NULL,value TEXT NOT NULL,version TEXT NOT NULL,identity TEXT NOT NULL,authority TEXT NOT NULL);
CREATE TABLE lww_retired(key TEXT PRIMARY KEY,stamp TEXT NOT NULL);
CREATE TABLE lww_publications(key TEXT NOT NULL,authority TEXT NOT NULL,version TEXT NOT NULL,stamp TEXT NOT NULL,identity TEXT NOT NULL,object_hash TEXT,PRIMARY KEY(key,authority));
CREATE TABLE lww_initialization_scopes(scope TEXT NOT NULL,authority TEXT NOT NULL,PRIMARY KEY(scope,authority));
CREATE TABLE lww_requests(request_id TEXT PRIMARY KEY,digest TEXT NOT NULL,revision INTEGER NOT NULL,activated_generation TEXT);
CREATE TABLE lww_receive_rows(request_id TEXT NOT NULL,key TEXT NOT NULL,stamp TEXT NOT NULL,value TEXT NOT NULL,status TEXT NOT NULL CHECK(status IN ('staged','held','deferred','done')),PRIMARY KEY(request_id,key));
CREATE INDEX lww_receive_status ON lww_receive_rows(status,key);
CREATE INDEX lww_units_scope ON lww_units(json_extract(key,'$[0]'),json_extract(key,'$[1]'),json_extract(key,'$[2]'),json_extract(key,'$[3]'));
CREATE INDEX lww_receive_scope ON lww_receive_rows(json_extract(key,'$[0]'),json_extract(key,'$[1]'),json_extract(key,'$[2]'),json_extract(key,'$[3]'));
"#;
pub(super) const DEVICE_SCHEMA: &str = r#"CREATE TABLE lww_clock(singleton INTEGER PRIMARY KEY CHECK(singleton=1),issued TEXT,accepted TEXT,binding_authority TEXT NOT NULL);
CREATE TABLE lww_intents(request_id TEXT PRIMARY KEY,authority TEXT NOT NULL,stamp TEXT NOT NULL,body TEXT NOT NULL,digest TEXT NOT NULL,complete INTEGER NOT NULL CHECK(complete IN (0,1)));
CREATE TABLE lww_intent_proofs(request_id TEXT PRIMARY KEY,identity TEXT NOT NULL);
CREATE TABLE lww_intent_rows(request_id TEXT NOT NULL,ordinal INTEGER NOT NULL,key TEXT NOT NULL,stamp TEXT,value TEXT NOT NULL,source_override INTEGER NOT NULL CHECK(source_override IN (0,1)),PRIMARY KEY(request_id,ordinal));
CREATE TABLE lww_intent_failures(request_id TEXT PRIMARY KEY,failures INTEGER NOT NULL CHECK(failures>0),error TEXT NOT NULL,quarantined INTEGER NOT NULL CHECK(quarantined IN (0,1)));
CREATE TABLE lww_receive(request_id TEXT PRIMARY KEY,authority TEXT NOT NULL,digest TEXT NOT NULL,body TEXT NOT NULL,applied INTEGER NOT NULL CHECK(applied IN (0,1)),finished INTEGER NOT NULL CHECK(finished IN (0,1)),kind TEXT NOT NULL,writer_id TEXT NOT NULL,cursor TEXT NOT NULL);
CREATE INDEX lww_receive_stream ON lww_receive(kind,writer_id);
CREATE TABLE lww_progress(kind TEXT NOT NULL,writer_id TEXT NOT NULL,cursor TEXT NOT NULL,authority TEXT NOT NULL,PRIMARY KEY(kind,writer_id));
CREATE TABLE lww_device_context(singleton INTEGER PRIMARY KEY CHECK(singleton=1),stamp TEXT NOT NULL,legacy_clock TEXT NOT NULL);
CREATE TABLE lww_unpublished_proofs(proof_id TEXT PRIMARY KEY,authority TEXT NOT NULL,entries TEXT NOT NULL);
CREATE TABLE lww_new_device_authorizations(authorization_id TEXT PRIMARY KEY,request_id TEXT NOT NULL UNIQUE,authority TEXT NOT NULL,staging_id TEXT NOT NULL,old_writer_id TEXT NOT NULL,writer_id TEXT NOT NULL,selection_change TEXT NOT NULL,authorized INTEGER NOT NULL CHECK(authorized IN (0,1)));
"#;

/// The receive rows a deferred drain applies again.
pub(super) const DRAIN_ROWS_SQL: &str = "SELECT request_id,key,stamp,value,status FROM lww_receive_rows WHERE status IN ('staged','held','deferred') ORDER BY key";
/// Replays of one intent that may fail before recovery sets it aside.
const INTENT_QUARANTINE_FAILURES: i64 = 3;
/// How much of a failure is kept with an intent that was set aside.
const INTENT_FAILURE_TEXT_CHARS: usize = 1024;

pub(super) fn error(message: impl ToString) -> StoreError {
    StoreError::Validation {
        message: message.to_string(),
    }
}
fn wire<T>(result: risunest_sync_wire::Result<T>) -> StoreResult<T> {
    result.map_err(error)
}
pub(super) fn inline(value: &Value) -> StoreResult<UnitValue> {
    wire(UnitValue::inline(&serde_json::to_vec(value)?))
}
/// A shared value whose canonical JSON exceeds the inline bound is stored as a
/// content object. The choice depends only on the bytes, so equal values have
/// equal units on every device.
pub(super) fn unit_value(db: &Connection, value: &Value) -> StoreResult<UnitValue> {
    use base64::Engine;
    let canonical = wire(risunest_sync_wire::payload_value::encode(value))?;
    if canonical.len() <= MAX_INLINE_UNIT_BYTES {
        return Ok(UnitValue::Inline {
            bytes: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(canonical),
        });
    }
    let hash = {
        #[cfg(test)]
        crate::persistent_store::hash_work::observe("native_large_unit_identity", canonical.len());
        risunest_sync_wire::hash(&canonical)
    };
    if !super::message_pages::retained_object_present(db, &hash)? {
        super::message_pages::put_object(db, &hash, &canonical)?;
    }
    wire(UnitValue::object(RecordDescriptor::content(hash)))
}
fn large_unit_body(db: &Connection, value: &UnitValue) -> StoreResult<Option<Vec<u8>>> {
    let UnitValue::Object { descriptor, .. } = value else {
        return Ok(None);
    };
    super::message_pages::object_body(db, &descriptor.object_hash)?
        .map(Some)
        .ok_or_else(|| error("unit-object-body-required"))
}
/// Reads a unit value, resolving a large unit from its stored body. Message
/// and archive units have their own object forms and are not read here.
pub(super) fn json_value_resolved(db: &Connection, value: &UnitValue) -> StoreResult<Option<Value>> {
    match large_unit_body(db, value)? {
        Some(body) => Ok(Some(serde_json::from_slice(&body)?)),
        None => json_value(value),
    }
}
/// A received large unit must be the content form of a body that could not
/// have been inline, so equal values keep equal identities across devices.
pub(super) fn validate_large_unit(db: &Connection, value: &UnitValue) -> StoreResult<Option<Value>> {
    let UnitValue::Object { descriptor, .. } = value else {
        return json_value(value);
    };
    if *descriptor != RecordDescriptor::content(descriptor.object_hash.clone()) {
        return Err(error("invalid-large-unit"));
    }
    let body = large_unit_body(db, value)?.ok_or_else(|| error("invalid-large-unit"))?;
    if body.len() <= MAX_INLINE_UNIT_BYTES
        || !risunest_sync_wire::payload_value::is_canonical(&body)
    {
        return Err(error("invalid-large-unit"));
    }
    Ok(Some(serde_json::from_slice(&body)?))
}
pub(super) fn unit_key(parts: &[&str]) -> StoreResult<UnitKey> {
    wire(UnitKey::new(parts))
}
pub(super) fn json_value(value: &UnitValue) -> StoreResult<Option<Value>> {
    use base64::Engine;
    match value {
        UnitValue::Inline { bytes } => Ok(Some(serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(bytes)
                .map_err(error)?,
        )?)),
        UnitValue::Deleted => Ok(None),
        UnitValue::Object { .. } => Err(error("unit-object-body-required")),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub(crate) enum UnitMutation {
    Set { key: UnitKey, value: Value },
    Delete { key: UnitKey },
}
impl UnitMutation {
    pub(super) fn key(&self) -> &UnitKey {
        match self {
            Self::Set { key, .. } | Self::Delete { key } => key,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MessageLocator {
    pub character_id: String,
    pub conversation_id: String,
    pub start: Option<i64>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Header {
    pub binding_authority: DecimalU64,
    pub request_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Change {
    pub key: UnitKey,
    pub stamp: Stamp,
    pub value: UnitValue,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OutboxEntry {
    pub key: UnitKey,
    pub stamp: Stamp,
    pub value: UnitValue,
    pub version: String,
    pub target_authority: DecimalU64,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OutboxPage {
    pub revision: i64,
    pub entries: Vec<OutboxEntry>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AckEntry {
    pub key: UnitKey,
    pub version: String,
    pub stamp: Stamp,
    pub value_identity: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Progress {
    pub kind: String,
    pub cursor: DecimalU64,
    pub writer_id: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StageReceive {
    #[serde(flatten)]
    pub header: Header,
    pub changes: Vec<Change>,
    pub progress: Progress,
    pub admitted_time_upper_ms: DecimalU64,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ApplyReceive {
    #[serde(flatten)]
    pub header: Header,
    #[serde(default)]
    pub generating: Vec<MessageLocator>,
}
#[derive(Debug, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ApplyResult {
    pub revision: i64,
    pub affected_keys: Vec<UnitKey>,
    pub plugin_storage_changed_keys: Vec<UnitKey>,
    pub held_keys: Vec<UnitKey>,
    pub deferred_keys: Vec<UnitKey>,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClockState {
    pub writer_id: String,
    pub issued: Option<Stamp>,
    pub accepted: Option<Stamp>,
    pub binding_authority: DecimalU64,
}
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NewDeviceResult {
    pub revision: i64,
    pub writer_id: String,
    pub binding_authority: DecimalU64,
}
#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NewDevicePreparation {
    pub authorization_id: String,
    pub writer_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum Intent<'a> {
    Commit {
        commit: WorkingSetCommit,
        aliases: Vec<super::AssetAlias>,
    },
    GuardedGalleryDelete {
        commit: WorkingSetCommit,
        aliases: Vec<super::AssetAlias>,
    },
    PluginClaim {
        owner: String,
        key: String,
        import_batch_id: String,
        assigned_at: i64,
        expected_revision: i64,
    },
    PluginAssign {
        sources: Vec<(String, String)>,
        to_owner: String,
        collision: commit::AssignCollision,
        assigned_at: i64,
        expected_revision: i64,
    },
    Replacement {
        device_revision: i64,
        staging_id: String,
        base_revision: i64,
        staging_digest: String,
        /// Rows of the changes, each marked when a source unit replaced the
        /// value captured from the stage.
        changes: intent_rows::IntentRows,
        /// Recognizes a retry with the same source units; nothing replays them.
        source_units: Option<intent_rows::IntentRows>,
        device_sections: Option<Cow<'a, device_store::sections::FrozenBackupSections>>,
        /// Fixed settings and permissions do not advance device_revision.
        device_settings_digest: Option<String>,
        device_changes: Cow<'a, [(UnitKey, UnitValue)]>,
    },
    Target {
        device_revision: i64,
        staging_id: String,
        changes: intent_rows::IntentRows,
    },
    NewDevice {
        device_revision: i64,
        authorization_id: String,
        staging_id: String,
        changes: Vec<Change>,
        old_writer_id: String,
        writer_id: String,
        new_authority: DecimalU64,
        selection_change: super::sync_selection::BindingSelectionChange,
    },
    Repair {
        entries: Vec<OutboxEntry>,
    },
    Archive {
        character_id: String,
        expected_revision: i64,
        at_ms: i64,
        restore: bool,
        incoming: Option<Change>,
    },
    Switch {
        device_revision: i64,
        change: super::sync_selection::BindingSelectionChange,
        new_authority: DecimalU64,
    },
}

/// An unfinished intent whose replay kept failing, so recovery no longer runs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QuarantinedIntent {
    pub(crate) request_id: String,
    pub(crate) kind: String,
    pub(crate) error: String,
    pub(crate) token: String,
    pub(crate) discardable: bool,
    pub(crate) recoverable: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum QuarantineDisposition { Delete, KeepReceipt, Unfinished }

fn quarantine_disposition(library: &Connection, device: &Connection, request_id: &str, body: &str, digest: &str) -> StoreResult<QuarantineDisposition> {
    let receipt: Option<(String,i64,Option<String>)> = library.query_row(
        "SELECT digest,revision,activated_generation FROM lww_requests WHERE request_id=?1", [request_id],
        |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
    ).optional()?;
    if let Some((committed_digest, revision, generation)) = receipt {
        if committed_digest != digest || risunest_sync_wire::hash(body.as_bytes()) != digest { return Ok(QuarantineDisposition::Unfinished); }
        return Ok(match serde_json::from_str::<Intent>(body) {
            Ok(Intent::Commit { .. } | Intent::GuardedGalleryDelete { .. } | Intent::PluginClaim { .. } | Intent::PluginAssign { .. } | Intent::Archive { .. }) => QuarantineDisposition::Delete,
            Ok(Intent::Replacement { staging_id, base_revision, device_sections: None, .. })
                if base_revision.checked_add(1) == Some(revision) && generation.as_deref() == Some(staging_id.as_str()) => QuarantineDisposition::KeepReceipt,
            _ => QuarantineDisposition::Unfinished,
        });
    }
    for db in [library, device] {
        let applied: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM lww_units WHERE version=?1) OR EXISTS(SELECT 1 FROM lww_outbox WHERE version=?1)",
            [request_id], |row| row.get(0),
        )?;
        if applied { return Ok(QuarantineDisposition::Unfinished); }
    }
    if device.query_row("SELECT EXISTS(SELECT 1 FROM lww_receive WHERE request_id=?1 AND applied=1)", [request_id], |row| row.get::<_,bool>(0))? {
        return Ok(QuarantineDisposition::Unfinished);
    }
    Ok(QuarantineDisposition::Delete)
}

impl Intent<'_> {
    fn for_commit(input: &WorkingSetCommit, aliases: &[super::AssetAlias]) -> Self {
        // Older readers ignore unknown commit fields, so guarded deletes need a kind they reject.
        if input.gallery_pending_fence.is_some() {
            Self::GuardedGalleryDelete { commit: input.clone(), aliases: aliases.to_vec() }
        } else {
            Self::Commit { commit: input.clone(), aliases: aliases.to_vec() }
        }
    }

    fn device_revision(&self) -> Option<i64> {
        match self {
            Self::Replacement { device_revision, device_sections: Some(_), .. }
            | Self::Target { device_revision, .. }
            | Self::NewDevice { device_revision, .. }
            | Self::Switch { device_revision, .. } => Some(*device_revision),
            _ => None,
        }
    }
    /// Whether the completed row stays. A retry of these kinds with the same request id is
    /// answered from it; the others are answered from the library receipt.
    fn kept_after_completion(&self) -> bool {
        matches!(
            self,
            Intent::Replacement { .. } | Intent::Target { .. } | Intent::NewDevice { .. } | Intent::Repair { .. } | Intent::Switch { .. }
        )
    }
}

#[cfg(test)]
#[derive(Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct CommitIntentInput(Intent<'static>);

#[cfg(test)]
std::thread_local! {
    static BACKUP_CAPTURE_PINNED: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    static BACKUP_CAPTURE_RELEASED: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

fn device_revision(db: &Connection) -> StoreResult<i64> {
    Ok(db.query_row("SELECT revision FROM device_meta WHERE singleton=1", [], |row| row.get(0))?)
}

fn intent_identity(authority: &str, stamp: &str, digest: &str) -> StoreResult<String> {
    let body = serde_json::to_string(&(authority, stamp, digest))?;
    #[cfg(test)]
    crate::persistent_store::hash_work::observe("native_intent_identity", body.len());
    Ok(risunest_sync_wire::hash(body.as_bytes()))
}

fn record_intent_identity(db: &Connection, request_id: &str) -> StoreResult<()> {
    let (authority, stamp, digest): (String, String, String) = db.query_row(
        "SELECT authority,stamp,digest FROM lww_intents WHERE request_id=?1", [request_id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
    )?;
    db.execute("INSERT INTO lww_intent_proofs VALUES(?1,?2)", params![request_id,intent_identity(&authority,&stamp,&digest)?])?;
    Ok(())
}

fn verify_device_intent_revision(db: &Connection, request_id: &str) -> StoreResult<()> {
    let (body, digest): (String, String) = db.query_row("SELECT body,digest FROM lww_intents WHERE request_id=?1", [request_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
    if risunest_sync_wire::hash(body.as_bytes()) != digest { return Err(error("request-id-integrity")); }
    let intent: Intent = serde_json::from_str(&body)?;
    if let Some(expected) = intent.device_revision() {
        if device_revision(db)? != expected { return Err(error("intent-device-changed")); }
    }
    verify_replacement_local_settings(db, &intent)?;
    Ok(())
}

fn verify_replacement_local_settings(db: &Connection, intent: &Intent) -> StoreResult<()> {
    if let Intent::Replacement { device_sections, device_settings_digest, .. } = intent {
        match (device_sections, device_settings_digest) {
            (Some(_), Some(expected)) => {
                if device_store::sections::replacement_local_settings_digest(db)? != *expected {
                    return Err(error("intent-device-changed"));
                }
            }
            (None, None) => {}
            _ => return Err(error("request-id-integrity")),
        }
    }
    Ok(())
}

pub(super) fn authority(db: &Connection) -> StoreResult<DecimalU64> {
    let value: String = db.query_row(
        "SELECT binding_authority FROM lww_clock WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    wire(value.try_into())
}
fn verify(db: &Connection, expected: DecimalU64) -> StoreResult<()> {
    if authority(db)? != expected {
        return Err(error("binding-authority-changed"));
    }
    Ok(())
}
fn validate_header(header: &Header) -> StoreResult<()> {
    if header.request_id.is_empty() {
        return Err(error("request-id-required"));
    }
    Ok(())
}
fn clock_state(db: &Connection) -> StoreResult<ClockState> {
    let (writer_id, issued, accepted, binding): (String, Option<String>, Option<String>, String) = db.query_row(
        "SELECT writer_id,issued,accepted,binding_authority FROM device_meta,lww_clock WHERE device_meta.singleton=1 AND lww_clock.singleton=1", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    Ok(ClockState {
        writer_id,
        issued: issued.map(|s| serde_json::from_str(&s)).transpose()?,
        accepted: accepted.map(|s| serde_json::from_str(&s)).transpose()?,
        binding_authority: wire(binding.try_into())?,
    })
}
#[cfg(test)]
pub(crate) mod test_clock {
    use super::*;
    use crate::external_storage::leases::LeaseClock;
    use std::sync::{Arc, Mutex, OnceLock, Weak};

    type Clocks = std::collections::BTreeMap<String, Weak<dyn LeaseClock>>;
    static CLOCKS: OnceLock<Mutex<Clocks>> = OnceLock::new();

    pub(crate) fn register(store: &PersistentStore, clock: Arc<dyn LeaseClock>) {
        let device = store.device_store().unwrap();
        let path = device.connection().path().unwrap().to_owned();
        let mut clocks = CLOCKS.get_or_init(Default::default).lock().unwrap();
        clocks.retain(|_, clock| clock.strong_count() > 0);
        clocks.insert(path, Arc::downgrade(&clock));
    }

    pub(super) fn now_ms(db: &Connection) -> Option<u64> {
        let clock = CLOCKS.get()?.lock().unwrap().get(db.path()?)?.upgrade()?;
        Some(clock.reading().wall_ms)
    }
}

fn stamp_wall_ms(_db: &Connection) -> StoreResult<u64> {
    #[cfg(test)]
    if let Some(now) = test_clock::now_ms(_db) { return Ok(now); }
    device_store::now_ms()?.try_into().map_err(error)
}

pub(super) fn reserve_stamp(db: &Connection) -> StoreResult<Stamp> {
    let state = clock_state(db)?;
    let stamp = wire(issue_stamp(
        stamp_wall_ms(db)?,
        &state.writer_id,
        state.issued.as_ref(),
        state.accepted.as_ref(),
    ))?;
    db.execute(
        "UPDATE lww_clock SET issued=?1 WHERE singleton=1",
        [serde_json::to_string(&stamp)?],
    )?;
    Ok(stamp)
}
fn observe(db: &Connection, stamp: &Stamp) -> StoreResult<()> {
    let state = clock_state(db)?;
    if state.accepted.as_ref().is_none_or(|old| old < stamp) {
        db.execute(
            "UPDATE lww_clock SET accepted=?1 WHERE singleton=1",
            [serde_json::to_string(stamp)?],
        )?;
    }
    Ok(())
}
pub(super) fn read_unit(db: &Connection, key: &UnitKey) -> StoreResult<Option<(Stamp, UnitValue)>> {
    let row: Option<(String, String)> = db
        .query_row(
            "SELECT stamp,value FROM lww_units WHERE key=?1",
            [key.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    row.map(|(stamp, value)| Ok((serde_json::from_str(&stamp)?, serde_json::from_str(&value)?)))
        .transpose()
}
pub(super) fn put_unit(
    db: &Connection,
    key: &UnitKey,
    stamp: &Stamp,
    value: &UnitValue,
    version: &str,
    publish: Option<DecimalU64>,
) -> StoreResult<()> {
    let identity = value_identity(value)?;
    let stamp = serde_json::to_string(stamp)?;
    let value = serde_json::to_string(value)?;
    db.execute("INSERT INTO lww_units VALUES(?1,?2,?3,?4,?5) ON CONFLICT(key) DO UPDATE SET stamp=excluded.stamp,value=excluded.value,version=excluded.version,identity=excluded.identity",params![key.as_str(),stamp,value,version,identity])?;
    if let Some(authority) = publish {
        db.execute("INSERT INTO lww_outbox VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(key) DO UPDATE SET stamp=excluded.stamp,value=excluded.value,version=excluded.version,identity=excluded.identity,authority=excluded.authority",params![key.as_str(),stamp,value,version,identity,authority.0.to_string()])?;
    } else {
        db.execute("DELETE FROM lww_outbox WHERE key=?1", [key.as_str()])?;
    }
    if key.components()[0] == "exists"
        && matches!(
            serde_json::from_str::<UnitValue>(&value)?,
            UnitValue::Deleted
        )
    {
        db.execute(
            "INSERT OR IGNORE INTO lww_retired VALUES(?1,?2)",
            params![key.as_str(), stamp],
        )?;
        suppress_retired(db, key)?;
    }
    Ok(())
}
fn parent_keys(key: &UnitKey) -> StoreResult<Vec<UnitKey>> {
    let c = key.components();
    let p: Vec<&str> = c.iter().map(String::as_str).collect();
    match p[0] {
        "character" | "archive" => {
            Ok(vec![unit_key(&["exists", "character", p[1]])?])
        }
        "conversation" | "messages" => Ok(vec![
            unit_key(&["exists", "character", p[1]])?,
            unit_key(&["exists", "conversation", p[1], p[2]])?,
        ]),
        "exists" if p[1] == "conversation" => Ok(vec![unit_key(&["exists", "character", p[2]])?]),
        "preset" | "persona" => Ok(vec![unit_key(&["exists", p[0], p[1]])?]),
        "record" if p[1] != "plugins" => Ok(vec![unit_key(&["exists", p[1], p[2]])?]),
        "order" if p[1] == "conversations" => Ok(vec![unit_key(&["exists", "character", p[2]])?]),
        _ => Ok(vec![]),
    }
}
fn is_retired(db: &Connection, key: &UnitKey) -> StoreResult<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM lww_retired WHERE key=?1)",
        [key.as_str()],
        |r| r.get(0),
    )?)
}
pub(super) fn parent_status(db: &Connection, key: &UnitKey) -> StoreResult<&'static str> {
    parent_status_in_generation(db,key,None)
}
fn parent_status_in_generation(db: &Connection, key: &UnitKey, generation: Option<&str>) -> StoreResult<&'static str> {
    if is_retired(db, key)? {
        return Ok("retired");
    }
    let mut missing = false;
    let archived_hold = key.components()[0] != "archive";
    for parent in parent_keys(key)? {
        if is_retired(db, &parent)? {
            return Ok("retired");
        }
        if read_unit(db, &parent)?.is_none() {
            missing = true;
        }
        if archived_hold && parent.components()[1] == "character" {
            let generation = generation.map(str::to_owned).map(Ok).unwrap_or_else(|| active_generation(db))?;
            let archived:Option<bool>=db.query_row("SELECT archived_object IS NOT NULL FROM characters WHERE generation=?1 AND character_id=?2",params![generation,parent.components()[2]],|r|r.get(0)).optional()?;
            if archived == Some(true) {
                missing = true;
            }
        }
    }
    Ok(if missing { "held" } else { "ready" })
}
/// The stage already contains captured values; project source overrides,
/// deletions and archives in dependency order, spooling pending detail patches.
pub(super) fn project_replacement_units(
    tx: &Transaction<'_>, generation: &str, header: &Header, stamp: &Stamp,
    changes: &FrozenRows,
) -> StoreResult<()> {
    changes.db.execute_batch("CREATE TABLE IF NOT EXISTS detail_changes(id TEXT PRIMARY KEY,value TEXT NOT NULL); DELETE FROM detail_changes;")?;
    changes.visit(true, |key, _, value, source_override| {
        if read_unit(tx, &key)? != Some((stamp.clone(), value.clone())) { return Ok(()); }
        let p = key.components();
        let hard_delete = p[0] == "exists" && matches!(value, UnitValue::Deleted);
        match parent_status_in_generation(tx, &key, Some(generation))? {
            "retired" if !hard_delete => return Ok(()),
            "held" => {
                tx.execute("INSERT INTO lww_receive_rows VALUES(?1,?2,?3,?4,'held')", params![header.request_id,key.as_str(),serde_json::to_string(stamp)?,serde_json::to_string(&value)?])?;
                return Ok(());
            }
            _ => {}
        }
        if !source_override && !matches!(value, UnitValue::Deleted) && p[0] != "archive" { return Ok(()); }
        if let Some(id) = projection::character_detail_key(&key) {
            let saved: Option<String> = changes.db.query_row("SELECT value FROM detail_changes WHERE id=?1", [&id], |row| row.get(0)).optional()?;
            let mut detail = match saved { Some(saved) => serde_json::from_str(&saved)?, None => projection::character_detail(tx, generation, &id)? };
            projection::patch_character_detail(tx, &mut detail, &key, &value)?;
            changes.db.execute("INSERT INTO detail_changes VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET value=excluded.value", params![id,serde_json::to_string(&detail)?])?;
        } else {
            if p[0] == "order" && p[1] == "conversations" {
                let saved: Option<String> = changes.db.query_row("SELECT value FROM detail_changes WHERE id=?1", [&p[2]], |row| row.get(0)).optional()?;
                if let Some(saved) = saved {
                    commit::put_character_detail(tx, generation, &serde_json::from_str(&saved)?)?;
                    changes.db.execute("DELETE FROM detail_changes WHERE id=?1", [&p[2]])?;
                }
            }
            projection::apply(tx, generation, &key, &value)?;
        }
        Ok(())
    })?;
    let mut details = changes.db.prepare("SELECT value FROM detail_changes ORDER BY id")?;
    let mut rows = details.query([])?;
    while let Some(row) = rows.next()? {
        commit::put_character_detail(tx, generation, &serde_json::from_str(&row.get::<_,String>(0)?)?)?;
    }
    Ok(())
}
fn suppress_retired(db: &Connection, parent: &UnitKey) -> StoreResult<()> {
    let p = parent.components();
    let (condition,first,second)=match p[1].as_str(){
        "character"=>("((json_extract(key,'$[0]') IN ('character','archive','conversation','messages') AND json_extract(key,'$[1]')=?1) OR (json_extract(key,'$[0]')='exists' AND json_extract(key,'$[1]')='conversation' AND json_extract(key,'$[2]')=?1) OR (json_extract(key,'$[0]')='order' AND json_extract(key,'$[1]')='conversations' AND json_extract(key,'$[2]')=?1))",p[2].as_str(),None),
        "conversation"=>("json_extract(key,'$[0]') IN ('conversation','messages') AND json_extract(key,'$[1]')=?1 AND json_extract(key,'$[2]')=?2",p[2].as_str(),Some(p[3].as_str())),
        "preset"|"persona"=>("json_extract(key,'$[0]')=?2 AND json_extract(key,'$[1]')=?1",p[2].as_str(),Some(p[1].as_str())),
        other=>("json_extract(key,'$[0]')='record' AND json_extract(key,'$[1]')=?2 AND json_extract(key,'$[2]')=?1",p[2].as_str(),Some(other)),
    };
    for (table, action) in [
        ("lww_units", "DELETE FROM lww_units"),
        ("lww_outbox", "DELETE FROM lww_outbox"),
        (
            "lww_receive_rows",
            "UPDATE lww_receive_rows SET status='done'",
        ),
    ] {
        let _ = table;
        let sql = format!("{action} WHERE {condition}");
        if second.is_some() {
            db.execute(&sql, params![first, second])?;
        } else {
            db.execute(&sql, [first])?;
        }
    }
    Ok(())
}

#[cfg(test)]
thread_local! { static WORK: std::cell::Cell<(u64,u64)> = const { std::cell::Cell::new((0,0)) }; }
#[cfg(test)]
pub(crate) fn reset_work_metrics() {
    WORK.with(|v| v.set((0, 0)));
}
#[cfg(test)]
pub(crate) fn take_work_metrics() -> (u64, u64) {
    WORK.with(|v| v.replace((0, 0)))
}
#[cfg(test)]
fn count_work() {
    WORK.with(|v| {
        let (u, b) = v.get();
        v.set((u + 1, b));
    });
}
fn value_identity(value: &UnitValue) -> StoreResult<String> {
    wire({
        let result = value.validate();
        #[cfg(test)]
        crate::persistent_store::hash_work::validation(&value);
        result
    })?;
    let bytes = wire(risunest_sync_wire::canonical::encode(value))?;
    #[cfg(test)]
    WORK.with(|v| {
        let (u, b) = v.get();
        v.set((u, b + bytes.len() as u64));
    });
    Ok({
        let hash_input = &bytes;
        #[cfg(test)]
        crate::persistent_store::hash_work::observe("native_unit_envelope", hash_input.len());
        risunest_sync_wire::hash(hash_input)
    })
}
pub(super) fn record_changes(
    tx: &Transaction<'_>,
    before: BTreeMap<UnitKey, UnitValue>,
    after: BTreeMap<UnitKey, UnitValue>,
    stamp: &Stamp,
    authority: DecimalU64,
    version: &str,
) -> StoreResult<Vec<UnitKey>> {
    let keys: BTreeSet<_> = before.keys().chain(after.keys()).cloned().collect();
    let mut changed = Vec::new();
    for key in keys {
        let value = after.get(&key).cloned().unwrap_or(UnitValue::Deleted);
        #[cfg(test)]
        count_work();
        if before.get(&key) != after.get(&key) {
            changed.push(key.clone());
        }
        if parent_status(tx, &key)? == "retired" {
            continue;
        }
        let prior = read_unit(tx, &key)?
            .map(|(_, v)| v)
            .or_else(|| before.get(&key).cloned())
            .unwrap_or(UnitValue::Deleted);
        if value_identity(&prior)? == value_identity(&value)? {
            continue;
        }
        put_unit(tx, &key, stamp, &value, version, Some(authority))?;
        ensure_publishable_parents(tx, &key, authority)?;
        if changed.last() != Some(&key) {
            changed.push(key);
        }
    }
    Ok(changed)
}

fn pin_backup_device_snapshot(library: &Connection, path: &str) -> StoreResult<()> {
    let mut uri=url::Url::from_file_path(path).map_err(|_| error("backup-device-path-unavailable"))?;
    uri.set_query(Some("mode=ro"));
    library.execute("ATTACH DATABASE ?1 AS backup_device", [uri.as_str()])?;
    if !library.is_readonly("backup_device")? { return Err(error("backup-device-snapshot-not-readonly")); }
    library.query_row("SELECT revision FROM backup_device.device_meta WHERE singleton=1", [], |row| row.get::<_,i64>(0))?;
    Ok(())
}

impl PersistentStore {
    #[cfg(test)]
    pub(crate) fn lww_backup_device_revision(&self, lease: &str) -> StoreResult<i64> {
        let reader = self.revision_leases.get(lease).ok_or(StoreError::SnapshotReleased)?;
        if current_revision(&reader.connection)? != reader.target.revision
            || active_generation(&reader.connection)? != reader.target.generation
        { return Err(error("backup-lease-identity-changed")); }
        let pinned: bool = reader.connection.query_row("SELECT EXISTS(SELECT 1 FROM pragma_database_list WHERE name='backup_device')", [], |row| row.get(0))?;
        if !pinned { return Err(error("backup-capture-lease-required")); }
        Ok(reader.connection.query_row("SELECT revision FROM backup_device.device_meta WHERE singleton=1", [], |row| row.get(0))?)
    }
    pub(crate) fn lww_visit_backup_units(&self, lease: &str, probe: &dyn crate::local_backup::CancellationProbe,
        emit: &mut dyn FnMut(UnitKey, UnitValue) -> StoreResult<()>) -> StoreResult<()> {
        let reader = self.revision_leases.get(lease).ok_or(StoreError::SnapshotReleased)?;
        if current_revision(&reader.connection)? != reader.target.revision
            || active_generation(&reader.connection)? != reader.target.generation
        { return Err(error("backup-lease-identity-changed")); }
        let pinned: bool = reader.connection.query_row("SELECT EXISTS(SELECT 1 FROM pragma_database_list WHERE name='backup_device')", [], |row| row.get(0))?;
        if !pinned { return Err(error("backup-capture-lease-required")); }
        let authority: String = reader.connection.query_row("SELECT binding_authority FROM backup_device.lww_clock WHERE singleton=1", [], |row| row.get(0))?;
        let authority: DecimalU64 = wire(authority.try_into())?;
        let mut statement = reader.connection.prepare("SELECT key,stamp,value FROM lww_units ORDER BY key")?;
        let rows = statement.query_map([], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?)))?;
        let scratch = crate::external_storage::leftovers::managed_scratch(&self.repository_root, "backup-units-")
            .map_err(|error| StoreError::Store { message: error.to_string() })?;
        let values = crate::sqlite_open::open(scratch.path().join("units.sqlite"))?;
        values.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA cache_size=-4096; PRAGMA temp_store=FILE;
            CREATE TABLE units(key TEXT PRIMARY KEY,stamp TEXT NOT NULL,value TEXT NOT NULL); BEGIN")?;
        'units: for row in rows {
            if probe.is_cancelled() { return Err(error("cancelled")); }
            let (key,stamp,value) = row?;
            let key: UnitKey = wire(key.try_into())?;
            for parent in parent_keys(&key)? {
                if is_retired(&reader.connection, &parent)? { continue 'units; }
            }
            let stamp: Stamp = serde_json::from_str(&stamp)?;
            wire(stamp.validate())?;
            let value: UnitValue = serde_json::from_str(&value)?;
            wire({
                let result = value.validate();
                #[cfg(test)]
                crate::persistent_store::hash_work::validation(&value);
                result
            })?;
            if !projection::is_device(&key) { values.execute("INSERT INTO units VALUES(?1,?2,?3)",params![key.as_str(),serde_json::to_string(&stamp)?,serde_json::to_string(&value)?])?; }
        }
        let mut statement = reader.connection.prepare("SELECT rows.request_id,rows.key,rows.stamp,rows.value,proof.digest,proof.body FROM lww_receive_rows rows JOIN backup_device.lww_receive proof ON proof.request_id=rows.request_id WHERE rows.status IN ('held','deferred') AND proof.applied=1 AND proof.finished=1 AND proof.authority=?1 ORDER BY rows.request_id,rows.key")?;
        let rows = statement.query_map([authority.0.to_string()], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,String>(3)?,row.get::<_,String>(4)?,row.get::<_,String>(5)?)))?;
        let mut proof: Option<(String,StageReceive)> = None;
        for row in rows {
            if probe.is_cancelled() { return Err(error("cancelled")); }
            let (request_id,key,stamp,value,digest,body) = row?;
            if proof.as_ref().is_none_or(|(id,_)| id != &request_id) {
                #[cfg(test)]
                crate::persistent_store::hash_work::observe("native_backup_receive_proof", body.len());
                if risunest_sync_wire::hash(body.as_bytes()) != digest { return Err(error("request-id-integrity")); }
                let request: StageReceive = serde_json::from_str(&body)?;
                if request.header.request_id != request_id || request.header.binding_authority != authority { return Err(error("request-id-integrity")); }
                proof = Some((request_id.clone(),request));
            }
            let change = Change { key:wire(key.try_into())?, stamp:serde_json::from_str(&stamp)?, value:serde_json::from_str(&value)? };
            let request = &proof.as_ref().unwrap().1;
            if !request.changes.contains(&change) || change.stamp.physical_ms > request.admitted_time_upper_ms { return Err(error("request-id-integrity")); }
            wire(change.stamp.validate())?;
            wire({
                let result = change.value.validate();
                #[cfg(test)]
                crate::persistent_store::hash_work::validation(&change.value);
                result
            })?;
            projection::validate_received(&reader.connection,&change.key,&change.value)?;
            if projection::is_device(&change.key) || parent_status(&reader.connection,&change.key)?=="retired" { continue; }
            let previous: Option<(String,String)> = values.query_row("SELECT stamp,value FROM units WHERE key=?1",[change.key.as_str()],|row| Ok((row.get(0)?,row.get(1)?))).optional()?;
            let decision = match previous {
                Some((stamp,value)) => wire({
                    let stamp: Stamp = serde_json::from_str(&stamp)?;
                    let value: UnitValue = serde_json::from_str(&value)?;
                    let result=compare_version(&stamp,&value,&change.stamp,&change.value);
                    #[cfg(test)]
                    crate::persistent_store::hash_work::comparison(&value,&change.value,&result);
                    result
                })?,
                None => LwwDecision::ApplyRemote,
            };
            if decision==LwwDecision::ApplyRemote { values.execute("INSERT INTO units VALUES(?1,?2,?3) ON CONFLICT(key) DO UPDATE SET stamp=excluded.stamp,value=excluded.value",params![change.key.as_str(),serde_json::to_string(&change.stamp)?,serde_json::to_string(&change.value)?])?; }
        }
        values.execute_batch("COMMIT")?;
        let mut query = values.prepare("SELECT key,value FROM units ORDER BY key")?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            if probe.is_cancelled() { return Err(error("cancelled")); }
            emit(wire(row.get::<_,String>(0)?.try_into())?,serde_json::from_str(&row.get::<_,String>(1)?)?)?;
        }
        Ok(())
    }
    pub(crate) fn lww_backup_unit_values(&self, lease: &str) -> StoreResult<BTreeMap<UnitKey, UnitValue>> {
        let mut values = BTreeMap::new();
        self.lww_visit_backup_units(lease, &crate::local_backup::NeverCancelled, &mut |key,value| { values.insert(key,value); Ok(()) })?;
        Ok(values)
    }
    pub(crate) fn lww_activated_receipt(&self, request_id: &str) -> StoreResult<Option<(i64, String)>> {
        activated_receipt(&self.connection, request_id)
    }

    pub(crate) fn lww_file_restore_receipt(&self, request_id: &str, staging_id: &str, authority: &str) -> StoreResult<Option<i64>> {
        let row: Option<(String, String, String, bool)> = self.device_store()?.connection().query_row(
            "SELECT authority,body,digest,complete FROM lww_intents WHERE request_id=?1", [request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).optional()?;
        let activated = activated_receipt(&self.connection, request_id)?;
        let Some((stored_authority, body, digest, complete)) = row else {
            return if activated.is_some() { Err(error("request-receipt-missing")) } else { Ok(None) };
        };
        let Intent::Replacement { staging_id: stage, base_revision, .. } = serde_json::from_str(&body)?
            else { return Err(error("request-id-integrity")); };
        if stored_authority != authority || stage != staging_id || risunest_sync_wire::hash(body.as_bytes()) != digest {
            return Err(error("request-id-integrity"));
        }
        let Some((revision, generation)) = activated else { return Err(error("restore-activation-pending")); };
        let applied_digest: String = self.connection.query_row(
            "SELECT digest FROM lww_requests WHERE request_id=?1", [request_id], |row| row.get(0),
        )?;
        if applied_digest != digest || generation != staging_id || base_revision.checked_add(1) != Some(revision) {
            return Err(error("request-id-integrity"));
        }
        if !complete { return Err(error("restore-activation-pending")); }
        Ok(Some(revision))
    }
    pub(crate) fn lww_device_replacement_receipt(
        &self,
        header: &Header,
        staging_id: &str,
    ) -> StoreResult<Option<RevisionResult>> {
        completed_device_replacement_receipt(&self.connection, self.device_store()?.connection(), header, staging_id)
    }
    /// Whether neither store holds an intent, a receipt or a written unit of `request_id`, so
    /// nothing under it was applied and nothing can be recovered from it.
    pub(crate) fn lww_request_unreserved(&self, request_id: &str) -> StoreResult<bool> {
        let device = self.device_store()?.connection();
        let reserved: bool = device.query_row(
            "SELECT EXISTS(SELECT 1 FROM lww_intents WHERE request_id=?1)", [request_id], |row| row.get(0),
        )?;
        if reserved || self.lww_request_recorded(request_id)? { return Ok(false); }
        for db in [&self.connection, device] {
            let written: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM lww_units WHERE version=?1) OR EXISTS(SELECT 1 FROM lww_outbox WHERE version=?1)",
                [request_id], |row| row.get(0),
            )?;
            if written { return Ok(false); }
        }
        Ok(true)
    }
    pub(crate) fn lww_acquire_backup_capture(
        &mut self,
        revision: i64,
    ) -> StoreResult<(super::LeaseResult, Vec<device_store::sections::PreparedSectionRows>)> {
        self.acquire_backup_capture(revision, true)
    }
    pub(crate) fn lww_acquire_library_backup_capture(&mut self, revision: i64) -> StoreResult<super::LeaseResult> {
        self.acquire_backup_capture(revision, false).map(|(lease, _)| lease)
    }
    fn acquire_backup_capture(
        &mut self,
        revision: i64,
        device_sections: bool,
    ) -> StoreResult<(super::LeaseResult, Vec<device_store::sections::PreparedSectionRows>)> {
        use risunest_external_storage_format::section::SectionKind;
        let path: String = self.device_store()?.connection().query_row(
            "SELECT file FROM pragma_database_list WHERE name='main'", [], |row| row.get(0),
        )?;
        if path.is_empty() { return Err(error("backup-device-path-unavailable")); }
        let mut barrier = crate::sqlite_open::open(&path)?;
        let tx = barrier.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|error| match error {
            rusqlite::Error::SqliteFailure(ref code, _) if matches!(code.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => StoreError::CommitBusy,
            error => error.into(),
        })?;
        if tx.query_row("SELECT EXISTS(SELECT 1 FROM lww_intents i WHERE complete=0 AND NOT EXISTS(SELECT 1 FROM lww_intent_failures f WHERE f.request_id=i.request_id AND f.quarantined=1)) OR EXISTS(SELECT 1 FROM lww_receive WHERE finished=0 AND authority=(SELECT binding_authority FROM lww_clock WHERE singleton=1))", [], |row| row.get::<_,bool>(0))? {
            return Err(StoreError::CommitBusy);
        }
        let snapshot = if device_sections {
            let snapshot = crate::sqlite_open::open_with_flags(
                &path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            snapshot.execute_batch("PRAGMA busy_timeout=5000; PRAGMA query_only=ON; PRAGMA mmap_size=0; BEGIN;")?;
            snapshot.query_row("SELECT revision FROM device_meta WHERE singleton=1", [], |row| row.get::<_,i64>(0))?;
            Some(snapshot)
        } else { None };
        let lease = self.acquire_revision(revision)?;
        let outcome = (|| {
            let library = &self.revision_leases[&lease.lease].connection;
            pin_backup_device_snapshot(library,&path)?;
            #[cfg(test)]
            BACKUP_CAPTURE_PINNED.with(|hook| { if let Some(hook) = hook.borrow_mut().take() { hook(); } });
            tx.rollback()?;
            #[cfg(test)]
            BACKUP_CAPTURE_RELEASED.with(|hook| { if let Some(hook) = hook.borrow_mut().take() { hook(); } });
            let sections = if let Some(snapshot) = snapshot {
                let sections = device_store::sections::capture_backup_sections_snapshot(
                    &snapshot, &[SectionKind::Hypa, SectionKind::LocalPlugins, SectionKind::LocalSettings],
                    &crate::external_storage::leftovers::scratch_directory(&self.repository_root)?,
                )?;
                snapshot.execute_batch("COMMIT;")?;
                sections
            } else { Vec::new() };
            Ok(sections)
        })();
        drop(barrier);
        match outcome {
            Ok(sections) => Ok((lease, sections)),
            Err(error) => { self.release_revision(&lease.lease)?; Err(error) }
        }
    }
    pub(crate) fn lww_object_body(&self, hash: &str) -> StoreResult<Option<Vec<u8>>> {
        if let Some(body) = super::message_pages::object_body(&self.connection, hash)? {
            return Ok(Some(body));
        }
        if let Some(body) = super::message_pages::object_body(self.device_store()?.connection(), hash)? {
            return Ok(Some(body));
        }
        use std::io::Read;
        let cas = crate::asset_repository::PayloadCas::new(&self.repository_root)?;
        let Some(mut file) = cas.open_object(hash)? else {
            return Ok(None);
        };
        let mut body = Vec::new();
        file.read_to_end(&mut body)?;
        if {
            let hash_input = &body;
            #[cfg(test)]
            crate::persistent_store::hash_work::observe("native_object_verify", hash_input.len());
            risunest_sync_wire::hash(hash_input)
        } != hash {
            return Err(error("object-hash-mismatch"));
        }
        Ok(Some(body))
    }

    pub(crate) fn lww_native_object(&self, hash: &str) -> StoreResult<Option<rusqlite::blob::Blob<'_>>> {
        for db in [&self.connection, self.device_store()?.connection()] {
            let row: Option<i64> = db.query_row("SELECT rowid FROM message_page_objects WHERE hash=?1", [hash], |row| row.get(0)).optional()?;
            if let Some(rowid) = row { return Ok(Some(db.blob_open(rusqlite::MAIN_DB, "message_page_objects", "body", rowid, true)?)); }
        }
        Ok(None)
    }

    pub(crate) fn lww_copy_large_object_to_cas(&self, hash: &str, cas: &crate::asset_repository::PayloadCas, inline_limit: usize) -> StoreResult<bool> {
        let Some(mut blob) = self.lww_native_object(hash)? else { return Ok(false); };
        let length = blob.len() as u64;
        if length <= inline_limit as u64 { return Ok(false); }
        cas.prepare_reader_expected(&mut blob, hash, length)?;
        Ok(true)
    }

    pub(crate) fn lww_verified_object_present(&self, hash: &str) -> StoreResult<bool> {
        super::message_pages::retained_object_present(&self.connection, hash)
    }
    /// The bodies a new value of `key` most likely shares bytes with: the value
    /// this device holds and the one last published under `binding`.
    pub(crate) fn lww_unit_object_bases(&self, key: &UnitKey, binding: DecimalU64) -> StoreResult<Vec<String>> {
        let db = if is_device(key) { self.device_store()?.connection() } else { &self.connection };
        let mut statement = db.prepare(
            "SELECT json_extract(value,'$.descriptor.objectHash') FROM lww_units WHERE key=?1 AND json_extract(value,'$.kind')='object'
            UNION SELECT object_hash FROM lww_publications WHERE key=?1 AND authority=?2 AND object_hash IS NOT NULL",
        )?;
        let rows = statement.query_map(params![key.as_str(), binding.0.to_string()], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
    /// Device units are applied inside the device store, so their large bodies
    /// are copied there from the library store where receive placed them.
    fn copy_device_unit_bodies<'a>(&self, changes: impl IntoIterator<Item = &'a Change>) -> StoreResult<()> {
        let device = self.device_store()?.connection();
        for change in changes {
            let UnitValue::Object { descriptor, .. } = &change.value else { continue };
            if !projection::is_device(&change.key)
                || super::message_pages::retained_object_present(device, &descriptor.object_hash)?
            {
                continue;
            }
            if change.key.components()[0] == "plugin-local"
                && device_store::plugin_values::raw_value::is_object(&self.connection, &descriptor.object_hash)? {
                let info = device_store::plugin_values::raw_value::inspect_object(&self.connection, &descriptor.object_hash)?;
                let tx = device.unchecked_transaction()?;
                device_store::plugin_values::raw_value::copy_object(&self.connection, &tx, &info)?;
                tx.commit()?;
                continue;
            }
            if change.key.components()[0] == "plugin-local"
                && device_store::plugin_values::text_value::object_row(&self.connection, &descriptor.object_hash)?.1
                    > device_store::plugin_values::text_value::PART_BYTES as u64
            {
                let info = device_store::plugin_values::text_value::inspect_object(&self.connection, &descriptor.object_hash)?;
                let tx = device.unchecked_transaction()?;
                device_store::plugin_values::text_value::copy_object(&self.connection, &tx, &info)?;
                tx.commit()?;
                continue;
            }
            let body = super::message_pages::object_body(&self.connection, &descriptor.object_hash)?
                .ok_or_else(|| error("unit-object-body-required"))?;
            super::message_pages::put_object(device, &descriptor.object_hash, &body)?;
        }
        Ok(())
    }
    pub(crate) fn lww_put_object(&self, hash: &str, body: &[u8]) -> StoreResult<()> {
        super::message_pages::put_object(&self.connection, hash, body)
    }
    pub(crate) fn lww_put_object_reader(&self, hash: &str, mut input: impl std::io::Read, length: u64) -> StoreResult<()> {
        use std::io::{Read, Write};
        use sha2::{Digest, Sha256};
        wire(risunest_sync_wire::validate_hash(hash))?;
        let length = i32::try_from(length).map_err(|_| error("object-size-invalid"))?;
        let db = &self.connection;
        db.execute_batch("SAVEPOINT lww_object_stream")?;
        let result = (|| -> StoreResult<()> {
            let existing: Option<i64> = db.query_row("SELECT rowid FROM message_page_objects WHERE hash=?1", [hash], |row| row.get(0)).optional()?;
            let rowid = match existing {
                Some(rowid) => rowid,
                None => {
                    db.execute("INSERT INTO message_page_objects(hash,body) VALUES(?1,zeroblob(?2))", params![hash, length])?;
                    db.last_insert_rowid()
                }
            };
            let mut output = db.blob_open(rusqlite::MAIN_DB, "message_page_objects", "body", rowid, existing.is_some())?;
            if output.len() != length as usize { return Err(error("message object identity collision")); }
            let mut buffer = [0; 64 * 1024];
            let mut digest = Sha256::new(); let mut copied = 0u64;
            loop {
                let count = input.read(&mut buffer)?;
                if count == 0 { break; }
                copied += count as u64;
                if copied > length as u64 { return Err(error("message object length mismatch")); }
                digest.update(&buffer[..count]);
                if existing.is_none() { output.write_all(&buffer[..count])?; }
            }
            #[cfg(test)] super::hash_work::observe("native_page_object_verify", copied as usize);
            if copied != length as u64 || hex::encode(digest.finalize()) != hash { return Err(error("message object hash mismatch")); }
            output.close()?;
            let mut stored = db.blob_open(rusqlite::MAIN_DB, "message_page_objects", "body", rowid, true)?;
            let mut digest = Sha256::new();
            loop { let count = stored.read(&mut buffer)?; if count == 0 { break; } digest.update(&buffer[..count]); }
            #[cfg(test)] super::hash_work::observe("native_page_object_verify", length as usize);
            if hex::encode(digest.finalize()) != hash { return Err(error("message object identity collision")); }
            stored.close()?;
            db.execute("INSERT INTO message_page_verified_objects(hash) SELECT ?1 WHERE NOT EXISTS(SELECT 1 FROM message_page_verified_objects WHERE hash=?1)", [hash])?;
            db.execute("DELETE FROM message_page_object_marks WHERE hash=?1", [hash])?;
            Ok(())
        })();
        match result {
            Ok(()) => { db.execute_batch("RELEASE lww_object_stream")?; Ok(()) }
            Err(failure) => { db.execute_batch("ROLLBACK TO lww_object_stream; RELEASE lww_object_stream")?; Err(failure) }
        }
    }
    pub(crate) fn lww_put_managed_object(&mut self, hash: &str, body: &[u8]) -> StoreResult<()> {
        if {
            let hash_input = body;
            #[cfg(test)]
            crate::persistent_store::hash_work::observe("native_object_verify", hash_input.len());
            risunest_sync_wire::hash(hash_input)
        } != hash {
            return Err(error("object-hash-mismatch"));
        }
        let cas = crate::asset_repository::PayloadCas::new(&self.repository_root)?;
        let prepared = cas.prepare_bytes(body)?;
        super::AssetObjectCatalog::new(&mut self.connection).register(
            &[super::asset_object_catalog::AssetObjectRegistration {
                object_hash: prepared.content_hash,
                byte_size: prepared.byte_size,
            }],
            device_store::now_ms()?,
        )
    }
    pub(crate) fn lww_receive_progress(&self, expected: DecimalU64) -> StoreResult<Vec<Progress>> {
        verify(self.device_store()?.connection(), expected)?;
        let db = self.device_store()?.connection();
        let mut s=db.prepare("SELECT kind,writer_id,cursor FROM lww_progress WHERE authority=?1 ORDER BY kind,writer_id")?;
        let rows = s.query_map([expected.0.to_string()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (kind, writer, cursor) = row?;
            out.push(Progress {
                kind,
                cursor: wire(cursor.try_into())?,
                writer_id: if writer.is_empty() {
                    None
                } else {
                    Some(writer)
                },
            });
        }
        Ok(out)
    }
    pub(crate) fn lww_binding_authority(&self) -> StoreResult<DecimalU64> {
        authority(self.device_store()?.connection())
    }
    pub(crate) fn lww_clock_state(&self) -> StoreResult<ClockState> {
        clock_state(self.device_store()?.connection())
    }
    fn intent_device_revision(&self, request_id: &str) -> StoreResult<i64> {
        let db = self.device_store()?.connection();
        let body: Option<String> = db.query_row("SELECT body FROM lww_intents WHERE request_id=?1", [request_id], |row| row.get(0)).optional()?;
        match body {
            Some(body) => serde_json::from_str::<Intent>(&body)?.device_revision().ok_or_else(|| error("request-id-integrity")),
            None => device_revision(db),
        }
    }

    fn verify_intent_device_revision(&self, request_id: &str) -> StoreResult<()> {
        verify_device_intent_revision(self.device_store()?.connection(), request_id)
    }

    fn reserve_intent(&mut self, header: &Header, intent: &Intent) -> StoreResult<(Stamp, String)> {
        validate_header(header)?;
        let body = serde_json::to_string(intent)?;
        let digest = {
            let hash_input = body.as_bytes();
            #[cfg(test)]
            {
                crate::persistent_store::hash_work::observe("native_intent", hash_input.len());
                crate::persistent_store::hash_work::record_commit_intent_input(hash_input);
            }
            risunest_sync_wire::hash(hash_input)
        };
        let tx = self.device_store_mut()?.transaction()?;
        verify(&tx, header.binding_authority)?;
        let previous: Option<(String, String)> = tx
            .query_row(
                "SELECT stamp,digest FROM lww_intents WHERE request_id=?1",
                [&header.request_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let stamp = if let Some((stamp, old)) = previous {
            if old != digest {
                return Err(error("request-id-integrity"));
            }
            serde_json::from_str(&stamp)?
        } else {
            verify_replacement_local_settings(&tx, intent)?;
            let stamp = reserve_stamp(&tx)?;
            tx.execute(
                "INSERT INTO lww_intents VALUES(?1,?2,?3,?4,?5,0)",
                params![
                    header.request_id,
                    header.binding_authority.0.to_string(),
                    serde_json::to_string(&stamp)?,
                    body,
                    digest
                ],
            )?;
            record_intent_identity(&tx, &header.request_id)?;
            stamp
        };
        tx.commit()?;
        Ok((stamp, digest))
    }
    fn complete_intent(&mut self, header: &Header, keep: bool) -> StoreResult<()> {
        let tx = self.device_store_mut()?.transaction()?;
        verify(&tx, header.binding_authority)?;
        if keep {
            tx.execute(
                "UPDATE lww_intents SET complete=1 WHERE request_id=?1",
                [&header.request_id],
            )?;
        } else {
            tx.execute("DELETE FROM lww_intents WHERE request_id=?1", [&header.request_id])?;
        }
        tx.execute("DELETE FROM lww_intent_rows WHERE request_id=?1", [&header.request_id])?;
        tx.execute("DELETE FROM lww_intent_failures WHERE request_id=?1", [&header.request_id])?;
        tx.execute("DELETE FROM lww_intent_proofs WHERE request_id=?1", [&header.request_id])?;
        tx.commit()?;
        Ok(())
    }
    fn completed_intent(
        &self,
        header: &Header,
        intent: &Intent,
    ) -> StoreResult<Option<RevisionResult>> {
        let digest = {
            let hash_input = serde_json::to_string(intent)?;
            #[cfg(test)]
            {
                crate::persistent_store::hash_work::observe("native_intent", hash_input.len());
                crate::persistent_store::hash_work::record_commit_intent_input(hash_input.as_bytes());
            }
            risunest_sync_wire::hash(hash_input.as_bytes())
        };
        let row: Option<(String, String, bool)> = self
            .device_store()?
            .connection()
            .query_row(
                "SELECT authority,digest,complete FROM lww_intents WHERE request_id=?1",
                [&header.request_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some((authority, old, complete)) = row {
            if authority != header.binding_authority.0.to_string() || old != digest {
                return Err(error("request-id-integrity"));
            }
            if complete {
                let (committed_digest, revision): (String, i64) = self.connection.query_row(
                    "SELECT digest,revision FROM lww_requests WHERE request_id=?1",
                    [&header.request_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                ).optional()?.ok_or_else(|| error("request-receipt-missing"))?;
                if committed_digest != digest
                    || matches!(intent, Intent::Replacement { base_revision, .. } if base_revision.checked_add(1) != Some(revision))
                {
                    return Err(error("request-id-integrity"));
                }
                return Ok(Some(RevisionResult { revision }));
            }
        }
        Ok(None)
    }
    pub(super) fn lww_commit(
        &mut self,
        input: &WorkingSetCommit,
        aliases: &[super::AssetAlias],
        committed_revision: &mut Option<i64>,
    ) -> StoreResult<RevisionResult> {
        self.lww_recover_intents()?;
        let header = Header {
            binding_authority: input
                .binding_authority
                .unwrap_or(self.lww_binding_authority()?),
            request_id: input
                .request_id
                .clone()
                .unwrap_or_else(|| Uuid::new_v4().to_string()),
        };
        let intent = Intent::for_commit(input, aliases);
        let (stamp, digest) = self.reserve_intent(&header, &intent)?;
        let result = commit::commit_lww(
            &mut self.connection,
            input,
            aliases,
            &header,
            &stamp,
            &digest,
        );
        *committed_revision = result.as_ref().ok().map(|value| value.revision);
        if result.is_ok()
            || matches!(
                result,
                Err(StoreError::RevisionConflict { .. }) | Err(StoreError::Validation { .. })
            )
        {
            self.complete_intent(&header, intent.kept_after_completion())?;
        }
        result
    }
    pub(super) fn lww_claim_plugin_value(
        &mut self,
        owner: &str,
        key: &str,
        import_batch_id: &str,
        assigned_at: i64,
        expected_revision: i64,
    ) -> StoreResult<(Option<Value>, i64)> {
        self.lww_recover_intents()?;
        if commit::plugin_claim_source(&self.connection, owner, key, import_batch_id, expected_revision)?.is_none() {
            return Ok((None, expected_revision));
        }
        let header = Header { binding_authority: self.lww_binding_authority()?, request_id: Uuid::new_v4().to_string() };
        let intent = Intent::PluginClaim {
            owner: owner.into(), key: key.into(), import_batch_id: import_batch_id.into(), assigned_at, expected_revision,
        };
        let (stamp, digest) = self.reserve_intent(&header, &intent)?;
        let result = commit::claim_unowned_plugin_value(
            &mut self.connection, owner, key, import_batch_id, assigned_at, expected_revision, &header, &stamp, &digest,
        );
        if result.is_ok() || matches!(result, Err(StoreError::RevisionConflict { .. }) | Err(StoreError::Validation { .. })) {
            self.complete_intent(&header, intent.kept_after_completion())?;
        }
        result
    }
    pub(super) fn lww_assign_plugin_storage(
        &mut self,
        sources: &[(String, String)],
        to_owner: &str,
        collision: commit::AssignCollision,
        assigned_at: i64,
        expected_revision: i64,
    ) -> StoreResult<(commit::AssignOutcome, i64)> {
        self.lww_recover_intents()?;
        commit::validate_assignment_owner(to_owner)?;
        if sources.is_empty() {
            return Ok((commit::AssignOutcome::default(), expected_revision));
        }
        let header = Header { binding_authority: self.lww_binding_authority()?, request_id: Uuid::new_v4().to_string() };
        let intent = Intent::PluginAssign {
            sources: sources.to_vec(), to_owner: to_owner.into(), collision, assigned_at, expected_revision,
        };
        let (stamp, digest) = self.reserve_intent(&header, &intent)?;
        let result = commit::assign_plugin_storage(
            &mut self.connection, sources, to_owner, collision, assigned_at, expected_revision, &header, &stamp, &digest,
        );
        if result.is_ok() || matches!(result, Err(StoreError::RevisionConflict { .. }) | Err(StoreError::Validation { .. })) {
            self.complete_intent(&header, intent.kept_after_completion())?;
        }
        result
    }
    pub(crate) fn lww_recover_intents(&mut self) -> StoreResult<()> {
        self.recover_intents(false).map(|_| ())
    }
    /// Replays unfinished intents while the store opens. A replay that fails is counted and
    /// waits for the next attempt instead of failing the open. Returns whether every intent
    /// recovery still runs has settled.
    pub(super) fn lww_recover_intents_at_open(&mut self) -> StoreResult<bool> {
        self.recover_intents(true)
    }
    fn recover_intents(&mut self, opening: bool) -> StoreResult<bool> {
        let authority = self.lww_binding_authority()?;
        let rows: Vec<(String, String, String, String, String)> = {
            let db = self.device_store()?.connection();
            // Completion already removes these rows, so most calls find nothing to delete and
            // take no write lock, which another connection may hold for a long write.
            let settled: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM lww_intent_rows WHERE request_id NOT IN (SELECT request_id FROM lww_intents WHERE complete=0))
                    OR EXISTS(SELECT 1 FROM lww_intent_proofs WHERE request_id NOT IN (SELECT request_id FROM lww_intents WHERE complete=0))
                    OR EXISTS(SELECT 1 FROM lww_intent_failures WHERE request_id NOT IN (SELECT request_id FROM lww_intents WHERE complete=0))",
                [], |r| r.get(0))?;
            if settled {
                intent_rows::delete_settled(db)?;
                db.execute("DELETE FROM lww_intent_proofs WHERE request_id NOT IN (SELECT request_id FROM lww_intents WHERE complete=0)", [])?;
                db.execute("DELETE FROM lww_intent_failures WHERE request_id NOT IN (SELECT request_id FROM lww_intents WHERE complete=0)", [])?;
            }
            let mut s=db.prepare("SELECT request_id,authority,stamp,body,digest FROM lww_intents i WHERE complete=0 AND NOT EXISTS(SELECT 1 FROM lww_intent_failures f WHERE f.request_id=i.request_id AND f.quarantined=1) ORDER BY rowid")?;
            let v = s
                .query_map([], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })?
                .collect::<Result<_, _>>()?;
            v
        };
        for (request_id, expected, stamp, body, digest) in rows {
            match self.recover_intent(authority, &request_id, expected, &stamp, &body, &digest) {
                Ok(()) => {}
                // These pass on their own, so they do not count toward setting the intent aside.
                Err(failure @ (StoreError::CommitBusy | StoreError::SnapshotReleased)) => {
                    if opening {
                        return Ok(false);
                    }
                    return Err(failure);
                }
                Err(failure) => {
                    if self.record_intent_failure(&request_id, &failure)? {
                        crate::nlog!("warn", "unfinished intent {} set aside after repeated failures: {}", request_id, failure);
                        continue;
                    }
                    if opening {
                        crate::nlog!("warn", "unfinished intent {} failed while opening: {}", request_id, failure);
                        return Ok(false);
                    }
                    return Err(failure);
                }
            }
        }
        Ok(true)
    }
    fn recover_intent(
        &mut self,
        authority: DecimalU64,
        request_id: &str,
        expected: String,
        stamp: &str,
        body: &str,
        digest: &str,
    ) -> StoreResult<()> {
        let header = Header {
            binding_authority: wire(expected.try_into())?,
            request_id: request_id.to_owned(),
        };
        let stamp = serde_json::from_str(stamp)?;
        let intent = serde_json::from_str::<Intent>(body)?;
        if risunest_sync_wire::hash(body.as_bytes()) != digest { return Err(error("request-id-integrity")); }
        let keep = intent.kept_after_completion();
        if let Intent::NewDevice {
            staging_id,
            changes,
            old_writer_id,
            writer_id,
            new_authority,
            selection_change,
            ..
        } = &intent
        {
            return self.finish_lww_new_device(
                &header,
                staging_id,
                changes,
                old_writer_id,
                writer_id,
                *new_authority,
                &stamp,
                digest,
                selection_change,
            );
        }
        if let Intent::Switch {
            change,
            new_authority,
            ..
        } = intent
        {
            return self.finish_lww_switch(&header, &change, new_authority, digest);
        }
        if header.binding_authority != authority {
            return Err(error("unfinished-intent-authority-changed"));
        }
        // A replay that the current state rejects is closed the way the live
        // call closes it, unless part of it already committed or its frozen
        // inputs no longer match.
        let closable = !matches!(&intent, Intent::Repair { .. });
        match self.replay_intent(&header, &stamp, digest, intent) {
            Ok(()) => {}
            Err(failure @ (StoreError::Validation { .. } | StoreError::RevisionConflict { .. }))
                if closable
                    && !breaks_intent_integrity(&failure)
                    && !self.lww_request_recorded(&header.request_id)? =>
            {
                crate::nlog!("warn", "unfinished intent {} closed without changes: {}", header.request_id, failure);
            }
            Err(failure) => return Err(failure),
        }
        self.complete_intent(&header, keep)
    }
    /// Counts a failed replay and sets the intent aside once it has failed
    /// `INTENT_QUARANTINE_FAILURES` times. Returns whether it is now set aside.
    fn record_intent_failure(&mut self, request_id: &str, failure: &StoreError) -> StoreResult<bool> {
        let text: String = failure.to_string().chars().take(INTENT_FAILURE_TEXT_CHARS).collect();
        let tx = self.device_store_mut()?.transaction()?;
        tx.execute(
            "INSERT INTO lww_intent_failures VALUES(?1,1,?2,0) ON CONFLICT(request_id) DO UPDATE SET failures=failures+1,error=excluded.error",
            params![request_id, text],
        )?;
        tx.execute(
            "UPDATE lww_intent_failures SET quarantined=1 WHERE request_id=?1 AND failures>=?2",
            params![request_id, INTENT_QUARANTINE_FAILURES],
        )?;
        let quarantined: bool = tx.query_row(
            "SELECT quarantined FROM lww_intent_failures WHERE request_id=?1",
            [request_id],
            |r| r.get(0),
        )?;
        tx.commit()?;
        Ok(quarantined)
    }
    pub(crate) fn lww_quarantined_intents(&self) -> StoreResult<Vec<QuarantinedIntent>> {
        let db = self.device_store()?.connection();
        let mut statement = db.prepare(
            "SELECT i.request_id,coalesce(CASE WHEN json_valid(i.body) THEN CAST(json_extract(i.body,'$.kind') AS TEXT) END,''),f.error,
             json_array(i.authority,i.stamp,i.body,i.digest,i.complete,f.failures,f.error,f.quarantined),i.body,i.digest
             FROM lww_intent_failures f JOIN lww_intents i ON i.request_id=f.request_id
             WHERE f.quarantined=1 AND i.complete=0 ORDER BY i.rowid",
        )?;
        let mut rows = statement.query([])?;
        let mut intents = Vec::new();
        while let Some(row) = rows.next()? {
            let request_id: String = row.get(0)?;
            let identity: String = row.get(3)?;
            let body: String = row.get(4)?;
            let digest: String = row.get(5)?;
            let disposition = quarantine_disposition(&self.connection, db, &request_id, &body, &digest)?;
            intents.push(QuarantinedIntent {
                recoverable: disposition == QuarantineDisposition::Unfinished && self.validate_quarantined_completion(&request_id).is_ok(),
                request_id, kind: row.get(1)?, error: row.get(2)?, token: risunest_sync_wire::hash(identity.as_bytes()),
                discardable: disposition != QuarantineDisposition::Unfinished,
            });
        }
        Ok(intents)
    }
    pub(crate) fn lww_discard_quarantined_intent(&mut self, request_id: &str, expected_token: &str, expected_revision: i64) -> StoreResult<bool> {
        let actual = current_revision(&self.connection)?;
        if actual != expected_revision { return Err(StoreError::RevisionConflict { expected: expected_revision, actual }); }
        let library = &self.connection;
        let device = self.device_store.as_mut().map_err(|failure| error(failure))?;
        let tx = device.transaction()?;
        let record: Option<(bool, Option<bool>, Option<String>, String, String)> = tx.query_row(
            "SELECT i.complete,f.quarantined,CASE WHEN f.request_id IS NOT NULL THEN json_array(i.authority,i.stamp,i.body,i.digest,i.complete,f.failures,f.error,f.quarantined) END,i.body,i.digest
             FROM lww_intents i LEFT JOIN lww_intent_failures f ON f.request_id=i.request_id WHERE i.request_id=?1",
            [request_id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)),
        ).optional()?;
        let Some((complete, quarantined, identity, body, digest)) = record else { return Ok(false); };
        if complete { return Ok(false); }
        if quarantined != Some(true) || identity.as_ref().map(|identity| risunest_sync_wire::hash(identity.as_bytes())).as_deref() != Some(expected_token) {
            return Err(error("stale-quarantined-intent"));
        }
        let disposition = quarantine_disposition(library, &tx, request_id, &body, &digest)?;
        if disposition == QuarantineDisposition::Unfinished { return Err(error("intent-partially-applied")); }
        tx.execute("DELETE FROM lww_intent_rows WHERE request_id=?1", [request_id])?;
        tx.execute("DELETE FROM lww_intent_failures WHERE request_id=?1", [request_id])?;
        tx.execute("DELETE FROM lww_intent_proofs WHERE request_id=?1", [request_id])?;
        if disposition == QuarantineDisposition::KeepReceipt {
            tx.execute("UPDATE lww_intents SET complete=1 WHERE request_id=?1", [request_id])?;
        } else {
            tx.execute("DELETE FROM lww_intents WHERE request_id=?1", [request_id])?;
        }
        tx.commit()?;
        self.invalidate_message_object_roots();
        Ok(true)
    }
    fn lww_request_recorded(&self, request_id: &str) -> StoreResult<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM lww_requests WHERE request_id=?1)",
            [request_id],
            |row| row.get(0),
        )?)
    }
    fn replay_intent(&mut self, header: &Header, stamp: &Stamp, digest: &str, intent: Intent) -> StoreResult<()> {
        let (header, stamp, digest) = (header.clone(), stamp.clone(), digest.to_owned());
        if matches!(&intent, Intent::Replacement { .. } | Intent::PluginClaim { .. } | Intent::PluginAssign { .. }) {
            self.completed_intent(&header, &intent)?;
        }
        match intent {
            Intent::Commit { commit: input, aliases }
            | Intent::GuardedGalleryDelete { commit: input, aliases } => {
                commit::commit_lww(
                    &mut self.connection,
                    &input,
                    &aliases,
                    &header,
                    &stamp,
                    &digest,
                )?;
            }
            Intent::PluginClaim { owner, key, import_batch_id, assigned_at, expected_revision } => {
                commit::claim_unowned_plugin_value(
                    &mut self.connection, &owner, &key, &import_batch_id, assigned_at, expected_revision,
                    &header, &stamp, &digest,
                )?;
            }
            Intent::PluginAssign { sources, to_owner, collision, assigned_at, expected_revision } => {
                commit::assign_plugin_storage(
                    &mut self.connection, &sources, &to_owner, collision, assigned_at, expected_revision,
                    &header, &stamp, &digest,
                )?;
            }
            Intent::Replacement {
                staging_id,
                changes,
                device_sections,
                device_changes,
                ..
            } => {
                let changes =
                    intent_rows::read_replacement(self.device_store()?.connection(), &self.repository_root, &header.request_id, &changes)?;
                if let Some(sections) = device_sections {
                    self.finish_lww_device_replacement(&header, &staging_id, &changes, &sections, &device_changes, &stamp, &digest)?;
                } else {
                    self.finish_lww_replacement(&header, &staging_id, &changes, &stamp, &digest, false)?;
                }
            }
            Intent::Target {
                staging_id,
                changes,
                ..
            } => {
                let changes = intent_rows::read_target(self.device_store()?.connection(), &self.repository_root, &header.request_id, &changes)?;
                self.finish_lww_replacement(
                    &header,
                    &staging_id,
                    &changes,
                    &stamp,
                    &digest,
                    true,
                )?;
            }
            Intent::Archive {
                character_id,
                expected_revision,
                at_ms,
                restore,
                incoming,
            } => {
                self.finish_lww_archive(
                    &header,
                    &stamp,
                    &digest,
                    &character_id,
                    expected_revision,
                    at_ms,
                    restore,
                    incoming.as_ref(),
                    &|| false,
                )?;
            }
            Intent::Repair { entries } => {
                self.finish_lww_repair(&header, &stamp, &entries)?;
            }
            Intent::Switch { .. } => unreachable!(),
            Intent::NewDevice { .. } => unreachable!(),
        }
        Ok(())
    }
    pub(super) fn lww_archive(
        &mut self,
        char_id: &str,
        expected_revision: i64,
        at_ms: i64,
        restore: bool,
        cancel: &dyn Fn() -> bool,
        cancellation: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> StoreResult<RevisionResult> {
        self.lww_recover_intents()?;
        // A body fetched here is in place before the intent exists, so a replay never fetches.
        super::archive::fetch_operation_body(&self.connection, &self.repository_root, char_id, restore, cancel, cancellation)?;
        let header = Header {
            binding_authority: self.lww_binding_authority()?,
            request_id: Uuid::new_v4().to_string(),
        };
        let intent = Intent::Archive {
            character_id: char_id.into(),
            expected_revision,
            at_ms,
            restore,
            incoming: None,
        };
        let (stamp, digest) = self.reserve_intent(&header, &intent)?;
        let result = self.finish_lww_archive(
            &header,
            &stamp,
            &digest,
            char_id,
            expected_revision,
            at_ms,
            restore,
            None,
            cancel,
        );
        if result.as_ref().is_err_and(|error| {
            error
                .to_string()
                .contains("character archive operation cancelled")
        }) || result.is_ok()
            || matches!(
                result,
                Err(StoreError::Validation { .. }) | Err(StoreError::RevisionConflict { .. })
            )
        {
            self.complete_intent(&header, intent.kept_after_completion())?;
        }
        result
    }
    fn finish_lww_archive(
        &mut self,
        header: &Header,
        stamp: &Stamp,
        digest: &str,
        char_id: &str,
        revision: i64,
        at_ms: i64,
        restore: bool,
        incoming: Option<&Change>,
        cancel: &dyn Fn() -> bool,
    ) -> StoreResult<RevisionResult> {
        let cas = crate::asset_repository::PayloadCas::new(&self.repository_root)?;
        let context = ArchiveContext {
            header,
            stamp: incoming.map(|c| &c.stamp).unwrap_or(stamp),
            digest,
            incoming,
        };
        let result = if restore {
            super::archive::restore_character_with_cancellation_lww(
                &mut self.connection,
                &cas,
                char_id,
                revision,
                cancel,
                Some(&context),
            )
        } else {
            super::archive::archive_character_with_cancellation_lww(
                &mut self.connection,
                &cas,
                char_id,
                revision,
                at_ms,
                cancel,
                Some(&context),
            )
        };
        if result.is_ok() {
            if let Some(incoming) = incoming {
                let tx = self.device_store_mut()?.transaction()?;
                observe(&tx, &incoming.stamp)?;
                tx.commit()?;
            }
        }
        result
    }
    pub(crate) fn lww_switch_target(
        &mut self,
        header: &Header,
        change: &super::sync_selection::BindingSelectionChange,
    ) -> StoreResult<DecimalU64> {
        let new_authority = DecimalU64(
            header
                .binding_authority
                .0
                .checked_add(1)
                .ok_or_else(|| error("binding-authority-exhausted"))?,
        );
        let intent = Intent::Switch {
            device_revision: self.intent_device_revision(&header.request_id)?,
            change: change.clone(),
            new_authority,
        };
        if self.completed_intent(header, &intent)?.is_some() {
            return Ok(new_authority);
        }
        self.lww_recover_intents()?;
        if self.completed_intent(header, &intent)?.is_some() {
            return Ok(new_authority);
        }
        let (_, digest) = self.reserve_intent(header, &intent)?;
        self.finish_lww_switch(header, change, new_authority, &digest)?;
        Ok(new_authority)
    }
    #[cfg(test)]
    pub(crate) fn lww_receive_row_counts(&self, request_id: &str) -> StoreResult<(i64, i64)> {
        let count = |db: &Connection| -> StoreResult<i64> {
            Ok(db.query_row("SELECT count(*) FROM lww_receive_rows WHERE request_id=?1", [request_id], |r| r.get(0))?)
        };
        Ok((count(&self.connection)?, count(self.device_store()?.connection())?))
    }
    #[cfg(test)]
    pub(crate) fn stop_next_switch_after_library_commit(&self) {
        SWITCH_LIBRARY_COMMITTED.with(|stop| stop.set(true));
    }
    fn finish_lww_switch(
        &mut self,
        header: &Header,
        change: &super::sync_selection::BindingSelectionChange,
        new: DecimalU64,
        digest: &str,
    ) -> StoreResult<()> {
        self.verify_intent_device_revision(&header.request_id)?;
        let current = self.lww_binding_authority()?;
        if current != header.binding_authority && current != new {
            return Err(error("binding-authority-changed"));
        }
        let retain = super::sync_selection::switch_retains_binding_state(&self.connection, &header.request_id)?;
        // The receipt keeps later library edits out of the reset on a device-side retry.
        let dropped = if retain {
            unfinished_receives(self.device_store()?.connection(), header.binding_authority)?
        } else {
            Vec::new()
        };
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let receipt: Option<String> = tx.query_row("SELECT digest FROM lww_requests WHERE request_id=?1", [&header.request_id], |row| row.get(0)).optional()?;
        if receipt.as_ref().is_some_and(|old| old != digest) { return Err(error("request-id-integrity")); }
        super::sync_selection::apply_binding_selection(&tx, change)?;
        if receipt.is_none() {
            tx.execute("DELETE FROM lww_initial_publication", [])?;
            if change.initial_publication {
                tx.execute("INSERT INTO lww_initial_publication VALUES(1,?1)", [new.0.to_string()])?;
            }
            if retain {
                carry_units(&tx, header.binding_authority, new)?;
                for request_id in &dropped {
                    tx.execute("DELETE FROM lww_receive_rows WHERE request_id=?1", [request_id])?;
                }
            } else {
                tx.execute("DELETE FROM lww_outbox", [])?;
                tx.execute("DELETE FROM lww_receive_rows", [])?;
            }
            tx.execute(
                "INSERT INTO lww_requests(request_id,digest,revision) VALUES(?1,?2,?3)",
                params![header.request_id, digest, current_revision(&tx)?],
            )?;
        } else if retain {
            carry_units(&tx, header.binding_authority, new)?;
        } else {
            // Only edits made after the recorded reset remain in this outbox.
            tx.execute("UPDATE lww_outbox SET authority=?2 WHERE authority=?1", params![header.binding_authority.0.to_string(), new.0.to_string()])?;
        }
        tx.commit()?;
        #[cfg(test)]
        if SWITCH_LIBRARY_COMMITTED.with(|stop| stop.replace(false)) {
            return Err(error("switch-stopped-after-library-commit"));
        }
        crate::server_sync::carry_operation_log(&self.repository_root, header.binding_authority, new, retain)
            .map_err(|failure| error(failure.code))?;
        let tx = self.device_store_mut()?.transaction()?;
        let active = authority(&tx)?;
        if active != header.binding_authority && active != new {
            return Err(error("binding-authority-changed"));
        }
        verify_device_intent_revision(&tx, &header.request_id)?;
        tx.execute(
            "UPDATE lww_clock SET binding_authority=?1 WHERE singleton=1",
            [new.0.to_string()],
        )?;
        if retain {
            carry_units(&tx, header.binding_authority, new)?;
            carry_device_progress(&tx, header.binding_authority, new)?;
            super::external_lww::carry_pending_publications(&tx, header.binding_authority, new)?;
        } else {
            tx.execute("DELETE FROM lww_outbox", [])?;
            tx.execute("DELETE FROM lww_receive", [])?;
            tx.execute("DELETE FROM lww_receive_rows", [])?;
            tx.execute("DELETE FROM lww_progress", [])?;
            tx.execute("DELETE FROM lww_unpublished_proofs", [])?;
        }
        tx.execute(
            "UPDATE lww_intents SET complete=1 WHERE request_id=?1",
            [&header.request_id],
        )?;
        tx.execute("DELETE FROM lww_intent_failures WHERE request_id=?1", [&header.request_id])?;
        tx.execute("DELETE FROM lww_intent_proofs WHERE request_id=?1", [&header.request_id])?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn lww_binding_extra_content(&self) -> StoreResult<(u64, Value)> {
        let mut count = 0u64;
        let mut vars = serde_json::Map::new();
        let mut s = self.connection.prepare("SELECT key,value FROM lww_units")?;
        let rows = s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (key, value) = row?;
            let key: UnitKey = wire(key.try_into())?;
            let value: UnitValue = serde_json::from_str(&value)?;
            if matches!(value, UnitValue::Deleted) {
                continue;
            }
            let p = key.components();
            if matches!(p[0].as_str(), "toggle" | "variable") {
                if let Some(value) = json_value_resolved(&self.connection, &value)? {
                    vars.insert(p[1].clone(), value);
                }
            }
            if !projection::known(&key) && !projection::is_device(&key) {
                count += 1;
            }
        }
        Ok((count, Value::Object(vars)))
    }
    pub(crate) fn lww_read_outbox(
        &self,
        expected: DecimalU64,
        limit: usize,
    ) -> StoreResult<OutboxPage> {
        self.lww_read_outbox_generating(expected, limit, &[])
    }
    /// Units waiting to be published, selected as `lww_read_outbox` selects them.
    pub(crate) fn lww_outbox_count(&self, expected: DecimalU64) -> StoreResult<u64> {
        verify(self.device_store()?.connection(), expected)?;
        let participates = projection::plugin_participates(self.device_store()?.connection())?;
        let mut count = 0;
        for db in [&self.connection, self.device_store()?.connection()] {
            count += db.query_row(
                "SELECT COUNT(*) FROM lww_outbox WHERE authority=?1 AND (?2 OR json_extract(key,'$[0]')<>'plugin-local')",
                params![expected.0.to_string(), participates],
                |r| r.get::<_, i64>(0),
            )? as u64;
        }
        Ok(count)
    }
    pub(crate) fn lww_read_outbox_generating(
        &self,
        expected: DecimalU64,
        limit: usize,
        generating: &[MessageLocator],
    ) -> StoreResult<OutboxPage> {
        verify(self.device_store()?.connection(), expected)?;
        let mut entries = Vec::new();
        for db in [&self.connection, self.device_store()?.connection()] {
            let mut s=db.prepare("SELECT key,stamp,value,version,authority FROM lww_outbox o WHERE authority=?1 AND (?3 OR json_extract(o.key,'$[0]')<>'plugin-local') AND (json_extract(o.key,'$[0]')<>'messages' OR NOT EXISTS(SELECT 1 FROM json_each(?4) g WHERE json_extract(g.value,'$.characterId')=json_extract(o.key,'$[1]') AND json_extract(g.value,'$.conversationId')=json_extract(o.key,'$[2]'))) ORDER BY key LIMIT ?2")?;
            let rows = s.query_map(
                params![
                    expected.0.to_string(),
                    i64::try_from(limit).map_err(error)?,
                    projection::plugin_participates(self.device_store()?.connection())?,
                    serde_json::to_string(generating)?
                ],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                },
            )?;
            for row in rows {
                let (key, stamp, value, version, authority) = row?;
                let key: UnitKey = wire(key.try_into())?;
                if key.components()[0] == "plugin-local"
                    && !projection::plugin_participates(self.device_store()?.connection())?
                {
                    continue;
                }
                entries.push(OutboxEntry {
                    key,
                    stamp: serde_json::from_str(&stamp)?,
                    value: serde_json::from_str(&value)?,
                    version,
                    target_authority: wire(authority.try_into())?,
                });
            }
        }
        entries.sort_by(|a, b| a.key.cmp(&b.key));
        entries.truncate(limit);
        Ok(OutboxPage {
            revision: self.revision()?,
            entries,
        })
    }
    pub(crate) fn lww_ack_outbox(
        &mut self,
        header: &Header,
        entries: &[AckEntry],
    ) -> StoreResult<()> {
        validate_header(header)?;
        verify(self.device_store()?.connection(), header.binding_authority)?;
        for db in [
            &mut self.connection,
            &mut self.device_store.as_mut().map_err(|e| error(e))?.connection,
        ] {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            for entry in entries {
                tx.execute("DELETE FROM lww_outbox WHERE key=?1 AND version=?2 AND stamp=?3 AND identity=?4 AND authority=?5",params![entry.key.as_str(),entry.version,serde_json::to_string(&entry.stamp)?,entry.value_identity,header.binding_authority.0.to_string()])?;
                witness_exact(
                    &tx,
                    &entry.key,
                    header.binding_authority,
                    &entry.version,
                    &entry.stamp,
                    &entry.value_identity,
                )?;
            }
            tx.commit()?;
        }
        let tx = self.device_store_mut()?.transaction()?;
        verify(&tx, header.binding_authority)?;
        for entry in entries {
            observe(&tx, &entry.stamp)?;
        }
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn lww_stage_receive(&mut self, request: &StageReceive) -> StoreResult<()> {
        validate_header(&request.header)?;
        verify(
            self.device_store()?.connection(),
            request.header.binding_authority,
        )?;
        if !matches!(request.progress.kind.as_str(), "server" | "external") {
            return Err(error("receive-progress-kind"));
        }
        if request.progress.kind == "external" {
            wire(risunest_sync_wire::stamp::validate_writer_id(
                request
                    .progress
                    .writer_id
                    .as_deref()
                    .ok_or_else(|| error("receive-writer-required"))?,
            ))?;
        }
        let mut keys = BTreeSet::new();
        for change in &request.changes {
            wire(change.stamp.validate())?;
            wire({
                let result = change.value.validate();
                #[cfg(test)]
                crate::persistent_store::hash_work::validation(&change.value);
                result
            })?;
            if !keys.insert(&change.key) {
                return Err(error("duplicate-receive-key"));
            }
            if change.stamp.physical_ms > request.admitted_time_upper_ms {
                return Err(error("incoming-clock-skew"));
            }
            if let Some((stamp, value)) = read_unit(
                if projection::is_device(&change.key) {
                    self.device_store()?.connection()
                } else {
                    &self.connection
                },
                &change.key,
            )? {
                wire({
                    let result = compare_version(
                        &stamp,
                        &value,
                        &change.stamp,
                        &change.value,
                    );
                    #[cfg(test)]
                    crate::persistent_store::hash_work::comparison(&value, &change.value, &result);
                    result
                })?;
            }
            projection::validate_received(&self.connection, &change.key, &change.value)?;
        }
        let body = serde_json::to_string(request)?;
        let digest = {
            let hash_input = body.as_bytes();
            #[cfg(test)]
            {
                crate::persistent_store::hash_work::observe("native_receive_intent", hash_input.len());
                crate::persistent_store::hash_work::record_receive_intent_input(hash_input);
            }
            risunest_sync_wire::hash(hash_input)
        };
        let tx = self.device_store_mut()?.transaction()?;
        verify(&tx, request.header.binding_authority)?;
        let old: Option<(String, bool)> = tx
            .query_row(
                "SELECT digest,finished FROM lww_receive WHERE request_id=?1",
                [&request.header.request_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if old.as_ref().is_some_and(|(old, _)| old != &digest) {
            return Err(error("request-id-integrity"));
        }
        // A finished receive is already applied, and its rows may be gone.
        if old.is_some_and(|(_, finished)| finished) {
            return Ok(());
        }
        tx.execute(
            "INSERT OR IGNORE INTO lww_receive(request_id,authority,digest,body,applied,finished,kind,writer_id,cursor) VALUES(?1,?2,?3,?4,0,0,?5,?6,?7)",
            params![
                request.header.request_id,
                request.header.binding_authority.0.to_string(),
                digest,
                body,
                request.progress.kind,
                request.progress.writer_id.as_deref().unwrap_or_default(),
                request.progress.cursor.0.to_string()
            ],
        )?;
        tx.commit()?;
        self.stage_receive_rows(request)?;
        Ok(())
    }
    fn stage_receive_rows(&mut self, request: &StageReceive) -> StoreResult<()> {
        self.copy_device_unit_bodies(&request.changes)?;
        for db in [
            &mut self.connection,
            &mut self.device_store.as_mut().map_err(|e| error(e))?.connection,
        ] {
            let device = db
                .path()
                .is_some_and(|p| p.ends_with(device_store::DEVICE_DATABASE_FILE));
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            for change in &request.changes {
                if projection::is_device(&change.key) != device {
                    continue;
                }
                tx.execute(
                    "INSERT OR IGNORE INTO lww_receive_rows VALUES(?1,?2,?3,?4,'staged')",
                    params![
                        request.header.request_id,
                        change.key.as_str(),
                        serde_json::to_string(&change.stamp)?,
                        serde_json::to_string(&change.value)?
                    ],
                )?;
            }
            tx.commit()?;
        }
        Ok(())
    }
    pub(crate) fn lww_apply_receive(&mut self, request: &ApplyReceive) -> StoreResult<ApplyResult> {
        verify(
            self.device_store()?.connection(),
            request.header.binding_authority,
        )?;
        let (body, finished): (String, bool) = self.device_store()?.connection().query_row(
            "SELECT body,finished FROM lww_receive WHERE request_id=?1 AND authority=?2",
            params![
                request.header.request_id,
                request.header.binding_authority.0.to_string()
            ],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let staged: StageReceive = serde_json::from_str(&body)?;
        if finished {
            return Ok(ApplyResult {
                revision: self.revision()?,
                ..Default::default()
            });
        }
        self.stage_receive_rows(&staged)?;
        let archived = self.restore_incoming_archives(&staged)?;
        let mut result = self.apply_receive_rows(&request.generating, false)?;
        result.affected_keys.extend(archived);
        let tx = self.device_store_mut()?.transaction()?;
        verify(&tx, request.header.binding_authority)?;
        for change in staged.changes {
            observe(&tx, &change.stamp)?;
        }
        tx.execute(
            "UPDATE lww_receive SET applied=1 WHERE request_id=?1",
            [&request.header.request_id],
        )?;
        tx.commit()?;
        if finished {
            result.revision = self.revision()?;
        }
        Ok(result)
    }
    /// Whether an incoming `archive` change restores (`Some(true)`) or archives (`Some(false)`)
    /// the character on this device, or changes nothing here.
    fn incoming_archive_transition(&self, change: &Change) -> StoreResult<Option<bool>> {
        let p = change.key.components();
        if p[0] != "archive" || parent_status(&self.connection, &change.key)? != "ready" {
            return Ok(None);
        }
        if let Some((stamp, value)) = read_unit(&self.connection, &change.key)? {
            if wire({
                let result = compare_version(
                    &stamp,
                    &value,
                    &change.stamp,
                    &change.value,
                );
                #[cfg(test)]
                crate::persistent_store::hash_work::comparison(&value, &change.value, &result);
                result
            })? != LwwDecision::ApplyRemote
            {
                return Ok(None);
            }
        }
        let generation = active_generation(&self.connection)?;
        let archived = super::archive::is_archived(&self.connection, &generation, &p[1])?;
        let restore = matches!(change.value, UnitValue::Deleted);
        Ok((restore == archived).then_some(restore))
    }
    /// The stored bodies the archives and restores in `changes` read on this device and that
    /// are not here: the archive a restore reads, or the owner manifest an archive lists.
    pub(crate) fn lww_incoming_archive_bodies(&self, changes: &[Change]) -> StoreResult<Vec<String>> {
        let cas = crate::asset_repository::PayloadCas::new(&self.repository_root)?;
        let mut missing = BTreeSet::new();
        for change in changes {
            let Some(restore) = self.incoming_archive_transition(change)? else {
                continue;
            };
            let character_id = &change.key.components()[1];
            let body = if restore {
                let generation = active_generation(&self.connection)?;
                super::archive::read_archived_object(&self.connection, &generation, character_id)?
                    .map(|archived| archived.object_hash)
            } else {
                super::archive::owner_manifest_hash(&self.connection, character_id)?
            };
            if let Some(body) = body.filter(|body| !missing.contains(body)) {
                if cas.stat_object(&body)?.is_none() {
                    missing.insert(body);
                }
            }
        }
        Ok(missing.into_iter().collect())
    }
    fn restore_incoming_archives(&mut self, staged: &StageReceive) -> StoreResult<Vec<UnitKey>> {
        self.lww_recover_intents()?;
        let mut affected = Vec::new();
        for change in &staged.changes {
            let Some(restore) = self.incoming_archive_transition(change)? else {
                continue;
            };
            let p = change.key.components();
            // A receive brings these bodies here before it is applied, so one that is still
            // missing fails the apply before anything changes.
            let cas = crate::asset_repository::PayloadCas::new(&self.repository_root)?;
            if restore {
                let generation = active_generation(&self.connection)?;
                let archived =
                    super::archive::read_archived_object(&self.connection, &generation, &p[1])?
                        .ok_or_else(|| error("archive-object-missing"))?;
                if cas.stat_object(&archived.object_hash)?.is_none() {
                    return Err(error("archive-object-missing"));
                }
            } else if let Some(manifest) =
                super::archive::owner_manifest_hash(&self.connection, &p[1])?
            {
                if cas.stat_object(&manifest)?.is_none() {
                    return Err(error("archive-manifest-missing"));
                }
            }
            let header = Header {
                binding_authority: staged.header.binding_authority,
                request_id: format!(
                    "archive-transition:{}:{}",
                    staged.header.request_id,
                    {
                        let hash_input = change.key.as_str().as_bytes();
                        #[cfg(test)]
                        crate::persistent_store::hash_work::observe("native_archive", hash_input.len());
                        risunest_sync_wire::hash(hash_input)
                    }
                ),
            };
            let expected_revision = self.revision()?;
            let at_ms = if restore {
                0
            } else {
                projection::archive_metadata(&self.connection, &change.value)?.archived_at
            };
            let intent = Intent::Archive {
                character_id: p[1].clone(),
                expected_revision,
                at_ms,
                restore,
                incoming: Some(change.clone()),
            };
            let (stamp, digest) = self.reserve_intent(&header, &intent)?;
            self.finish_lww_archive(
                &header,
                &stamp,
                &digest,
                &p[1],
                expected_revision,
                at_ms,
                restore,
                Some(change),
                &|| false,
            )?;
            self.complete_intent(&header, intent.kept_after_completion())?;
            affected.push(change.key.clone());
        }
        Ok(affected)
    }

    fn apply_receive_rows(
        &mut self,
        generating: &[MessageLocator],
        drain: bool,
    ) -> StoreResult<ApplyResult> {
        let current_authority = self.lww_binding_authority()?;
        let mut result = ApplyResult::default();
        for db in [
            &mut self.connection,
            &mut self.device_store.as_mut().map_err(|e| error(e))?.connection,
        ] {
            let device = db
                .path()
                .is_some_and(|p| p.ends_with(device_store::DEVICE_DATABASE_FILE));
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let generation = if device {
                String::new()
            } else {
                active_generation(&tx)?
            };
            let mut changes: Vec<(String, Change, String)> = {
                let mut s=tx.prepare(if drain {DRAIN_ROWS_SQL}else{"SELECT request_id,key,stamp,value,status FROM lww_receive_rows WHERE status IN ('staged','held') ORDER BY key"})?;
                let rows = s.query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                })?;
                let mut v = Vec::new();
                for row in rows {
                    let (id, key, stamp, value, status) = row?;
                    v.push((
                        id,
                        Change {
                            key: wire(key.try_into())?,
                            stamp: serde_json::from_str(&stamp)?,
                            value: serde_json::from_str(&value)?,
                        },
                        status,
                    ));
                }
                v
            };
            changes.sort_by_key(|(_, c, _)| {
                if c.key.components()[0] == "exists" {
                    0
                } else {
                    1
                }
            });
            let revision = if device {
                device_store::begin_mutation_remote(&tx)?
            } else {
                let revision = current_revision(&tx)? + 1;
                super::content_change_index::begin_mutation(&tx, &generation, revision, "server")?;
                revision
            };
            let mut worked = false;
            let affected_before = result.affected_keys.len();
            for (id, change, previous_status) in changes {
                let c = change.key.components();
                let was_unprojected = matches!(previous_status.as_str(), "held" | "deferred");
                let parent = parent_status(&tx, &change.key)?;
                let retired = parent == "retired";
                let status = if retired {
                    "done"
                } else if !device && parent == "held" {
                    "held"
                } else if c[0] == "messages"
                    && generating
                        .iter()
                        .any(|g| g.character_id == c[1] && g.conversation_id == c[2])
                {
                    "deferred"
                } else {
                    "done"
                };
                let hard_delete = c[0] == "exists" && matches!(change.value, UnitValue::Deleted);
                if status == "done" && (!retired || hard_delete) {
                    let decision = if let Some((stamp, value)) = read_unit(&tx, &change.key)? {
                        wire({
                            let result = compare_version(
                                &stamp,
                                &value,
                                &change.stamp,
                                &change.value,
                            );
                            #[cfg(test)]
                            crate::persistent_store::hash_work::comparison(&value, &change.value, &result);
                            result
                        })?
                    } else {
                        LwwDecision::ApplyRemote
                    };
                    let new_retirement = hard_delete
                        && read_unit(&tx, &change.key)?
                            .is_none_or(|(_, v)| !matches!(v, UnitValue::Deleted));
                    if decision == LwwDecision::ApplyRemote || new_retirement {
                        if projection::apply_received(&tx, &generation, &change.key, &change.value)? {
                            result.plugin_storage_changed_keys.push(change.key.clone());
                        }
                        put_unit(
                            &tx,
                            &change.key,
                            &change.stamp,
                            &change.value,
                            &format!("receive:{id}"),
                            None,
                        )?;
                        result.affected_keys.push(change.key.clone());
                    } else if was_unprojected && decision == LwwDecision::Identical {
                        if projection::apply_received(&tx, &generation, &change.key, &change.value)? {
                            result.plugin_storage_changed_keys.push(change.key.clone());
                        }
                        result.affected_keys.push(change.key.clone());
                    }
                    let local_pending: bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM lww_requests receipt JOIN lww_outbox pending ON pending.version=receipt.request_id WHERE receipt.request_id=?1 AND pending.key=?2 AND pending.stamp=?3 AND pending.value=?4)",params![id,change.key.as_str(),serde_json::to_string(&change.stamp)?,serde_json::to_string(&change.value)?],|row|row.get(0))?;
                    if !local_pending { witness_received(&tx, &change, current_authority)?; }
                }
                if status != previous_status {
                    worked = true;
                    tx.execute(
                        "UPDATE lww_receive_rows SET status=?3 WHERE request_id=?1 AND key=?2",
                        params![id, change.key.as_str(), status],
                    )?;
                }
                if status == "held" {
                    result.held_keys.push(change.key);
                } else if status == "deferred" {
                    result.deferred_keys.push(change.key);
                }
            }
            // Rows that stay held or deferred change nothing, so the revision stays.
            if !worked {
                continue;
            }
            if device {
                device_store::finish_mutation_remote(&tx)?;
            } else if result.affected_keys.len() == affected_before {
                // Only row status moved (an echo of this device's own writes, or
                // rows now held), so the library content and its revision stay.
                super::content_change_index::finish_mutation(&tx)?;
            } else {
                projection::refresh_orders(&tx, &generation, &result.affected_keys)?;
                super::content_change_index::finish_mutation(&tx)?;
                commit::set_active(&tx, revision, &generation)?;
            }
            tx.commit()?;
        }
        result.revision = self.revision()?;
        Ok(result)
    }
    pub(crate) fn lww_finish_receive(&mut self, header: &Header) -> StoreResult<()> {
        let tx = self.device_store_mut()?.transaction()?;
        verify(&tx, header.binding_authority)?;
        let (body, applied): (String, bool) = tx.query_row(
            "SELECT body,applied FROM lww_receive WHERE request_id=?1",
            [&header.request_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if !applied {
            return Err(error("receive-not-applied"));
        }
        let request: StageReceive = serde_json::from_str(&body)?;
        let writer = request.progress.writer_id.unwrap_or_default();
        let old: Option<String> = tx
            .query_row(
                "SELECT cursor FROM lww_progress WHERE kind=?1 AND writer_id=?2",
                params![request.progress.kind, writer],
                |r| r.get(0),
            )
            .optional()?;
        // A missing cursor counts as 0, so a page that keeps the cursor where it
        // is writes no progress.
        if old
            .map(|c| wire(c.try_into()))
            .transpose()?
            .unwrap_or(DecimalU64(0))
            < request.progress.cursor
        {
            tx.execute("INSERT INTO lww_progress VALUES(?1,?2,?3,?4) ON CONFLICT(kind,writer_id) DO UPDATE SET cursor=excluded.cursor,authority=excluded.authority",params![request.progress.kind,writer,request.progress.cursor.0.to_string(),header.binding_authority.0.to_string()])?;
        }
        tx.execute(
            "UPDATE lww_receive SET finished=1 WHERE request_id=?1",
            [&header.request_id],
        )?;
        tx.commit()?;
        self.prune_receive_history()
    }
    /// Drops the receive history no proof needs. A finished receive stays while a held or
    /// deferred row points at it, or until a later finished receive of its progress stream
    /// moves the cursor. Pages that repeat the cursor belong to a group still arriving, and
    /// acknowledging the last page reads it back. `done` rows go once their receive has
    /// finished.
    fn prune_receive_history(&mut self) -> StoreResult<()> {
        let unfinished: Vec<String> = {
            let device = self.device_store()?.connection();
            let mut statement = device.prepare("SELECT request_id FROM lww_receive WHERE finished=0")?;
            let ids = statement.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
            ids
        };
        let unfinished = serde_json::to_string(&unfinished)?;
        for db in [&self.connection, self.device_store()?.connection()] {
            db.execute(
                "DELETE FROM lww_receive_rows WHERE status='done' AND request_id NOT IN (SELECT value FROM json_each(?1))",
                [&unfinished],
            )?;
        }
        let superseded: Vec<String> = {
            let device = self.device_store()?.connection();
            let mut statement = device.prepare(
                "SELECT request_id FROM lww_receive old WHERE finished=1 AND EXISTS(
                     SELECT 1 FROM lww_receive newer WHERE newer.finished=1 AND newer.kind=old.kind
                     AND newer.writer_id=old.writer_id AND newer.rowid>old.rowid AND newer.cursor<>old.cursor)",
            )?;
            let ids = statement.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?;
            ids
        };
        for request_id in superseded {
            let mut proof = false;
            for db in [&self.connection, self.device_store()?.connection()] {
                proof |= db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM lww_receive_rows WHERE request_id=?1 AND status IN ('held','deferred'))",
                    [&request_id],
                    |r| r.get::<_, bool>(0),
                )?;
            }
            if !proof {
                self.device_store()?.connection().execute(
                    "DELETE FROM lww_receive WHERE request_id=?1 AND finished=1",
                    [&request_id],
                )?;
            }
        }
        Ok(())
    }
    pub(crate) fn lww_drain_deferred(
        &mut self,
        request: &ApplyReceive,
    ) -> StoreResult<ApplyResult> {
        verify(
            self.device_store()?.connection(),
            request.header.binding_authority,
        )?;
        let result = self.apply_receive_rows(&request.generating, true)?;
        self.prune_receive_history()?;
        Ok(result)
    }
    pub(crate) fn lww_record_unpublished_proof(
        &mut self,
        header: &Header,
        proof_id: &str,
        entries: &[AckEntry],
    ) -> StoreResult<()> {
        let tx = self.device_store_mut()?.transaction()?;
        verify(&tx, header.binding_authority)?;
        let body = serde_json::to_string(entries)?;
        let old: Option<String> = tx
            .query_row(
                "SELECT entries FROM lww_unpublished_proofs WHERE proof_id=?1",
                [proof_id],
                |r| r.get(0),
            )
            .optional()?;
        if old.as_ref().is_some_and(|old| old != &body) {
            return Err(error("unpublished-proof-integrity"));
        }
        tx.execute(
            "INSERT OR IGNORE INTO lww_unpublished_proofs VALUES(?1,?2,?3)",
            params![proof_id, header.binding_authority.0.to_string(), body],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub(crate) fn lww_retry_unpublished(
        &mut self,
        header: &Header,
        proof_id: &str,
        corrected: DecimalU64,
    ) -> StoreResult<ApplyResult> {
        let existing: Option<(String, String, bool)> = self
            .device_store()?
            .connection()
            .query_row(
                "SELECT authority,body,complete FROM lww_intents WHERE request_id=?1",
                [&header.request_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some((authority, body, complete)) = existing {
            if authority != header.binding_authority.0.to_string() {
                return Err(error("request-id-integrity"));
            }
            let intent: Intent = serde_json::from_str(&body)?;
            let Intent::Repair { entries } = intent else {
                return Err(error("request-id-integrity"));
            };
            if !complete {
                self.lww_recover_intents()?;
                let settled: bool = self.device_store()?.connection().query_row(
                    "SELECT complete FROM lww_intents WHERE request_id=?1",
                    [&header.request_id],
                    |r| r.get(0),
                )?;
                if !settled {
                    return Err(error("intent-quarantined"));
                }
            }
            return Ok(ApplyResult {
                revision: self.revision()?,
                affected_keys: entries.into_iter().map(|e| e.key).collect(),
                ..Default::default()
            });
        }
        self.lww_recover_intents()?;
        verify(self.device_store()?.connection(), header.binding_authority)?;
        validate_header(header)?;
        let body: String = self.device_store()?.connection().query_row(
            "SELECT entries FROM lww_unpublished_proofs WHERE proof_id=?1 AND authority=?2",
            params![proof_id, header.binding_authority.0.to_string()],
            |r| r.get(0),
        )?;
        let proof: Vec<AckEntry> = serde_json::from_str(&body)?;
        let state = self.lww_clock_state()?;
        let upper = corrected
            .0
            .checked_add(risunest_sync_wire::stamp::MAX_CLOCK_SKEW_MS)
            .ok_or_else(|| error("clock-overflow"))?;
        if state
            .accepted
            .as_ref()
            .is_some_and(|s| s.physical_ms.0 > upper)
        {
            return Err(error("accepted-clock-correction-required"));
        }
        let entries = self
            .lww_read_all_outbox(header.binding_authority)?
            .into_iter()
            .filter(|e| e.stamp.physical_ms.0 > upper)
            .collect::<Vec<_>>();
        for entry in &entries {
            if !proof.iter().any(|p| {
                p.key == entry.key
                    && p.version == entry.version
                    && p.stamp == entry.stamp
                    && {
                        let result = entry.value.identity();
                        #[cfg(test)]
                        crate::persistent_store::hash_work::identity(&entry.value, &result);
                        result
                    }.ok().as_ref() == Some(&p.value_identity)
            }) {
                return Err(error("unpublished-proof-incomplete"));
            }
        }
        let intent = Intent::Repair {
            entries: entries.clone(),
        };
        let body = serde_json::to_string(&intent)?;
        let digest = {
            let hash_input = body.as_bytes();
            #[cfg(test)]
            crate::persistent_store::hash_work::observe("native_retry_intent", hash_input.len());
            risunest_sync_wire::hash(hash_input)
        };
        let tx = self.device_store_mut()?.transaction()?;
        verify(&tx, header.binding_authority)?;
        let stamp = wire(issue_stamp(
            corrected.0,
            &state.writer_id,
            None,
            state.accepted.as_ref(),
        ))?;
        tx.execute(
            "UPDATE lww_clock SET issued=?1 WHERE singleton=1",
            [serde_json::to_string(&stamp)?],
        )?;
        tx.execute(
            "INSERT INTO lww_intents VALUES(?1,?2,?3,?4,?5,0)",
            params![
                header.request_id,
                header.binding_authority.0.to_string(),
                serde_json::to_string(&stamp)?,
                body,
                digest
            ],
        )?;
        record_intent_identity(&tx, &header.request_id)?;
        tx.commit()?;
        self.finish_lww_repair(header, &stamp, &entries)?;
        self.complete_intent(header, intent.kept_after_completion())?;
        Ok(ApplyResult {
            revision: self.revision()?,
            affected_keys: entries.into_iter().map(|e| e.key).collect(),
            ..Default::default()
        })
    }
    fn lww_read_all_outbox(&self, expected: DecimalU64) -> StoreResult<Vec<OutboxEntry>> {
        let mut entries = Vec::new();
        for db in [&self.connection, self.device_store()?.connection()] {
            let mut s =
                db.prepare("SELECT key,stamp,value,version FROM lww_outbox WHERE authority=?1")?;
            let rows = s.query_map([expected.0.to_string()], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?;
            for row in rows {
                let (key, stamp, value, version) = row?;
                entries.push(OutboxEntry {
                    key: wire(key.try_into())?,
                    stamp: serde_json::from_str(&stamp)?,
                    value: serde_json::from_str(&value)?,
                    version,
                    target_authority: expected,
                });
            }
        }
        Ok(entries)
    }
    fn finish_lww_repair(
        &mut self,
        header: &Header,
        stamp: &Stamp,
        entries: &[OutboxEntry],
    ) -> StoreResult<()> {
        for db in [
            &mut self.connection,
            &mut self.device_store.as_mut().map_err(|e| error(e))?.connection,
        ] {
            let device = db
                .path()
                .is_some_and(|p| p.ends_with(device_store::DEVICE_DATABASE_FILE));
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if device { verify(&tx, header.binding_authority)?; }
            for entry in entries {
                if projection::is_device(&entry.key) != device {
                    continue;
                }
                let row: Option<(String, String, String, String)> = tx
                    .query_row(
                        "SELECT version,stamp,value,authority FROM lww_outbox WHERE key=?1",
                        [entry.key.as_str()],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .optional()?;
                let Some((version, current_stamp, value, authority)) = row else { return Err(error("unpublished-version-changed")); };
                let unit: Option<(String, String, String)> = tx.query_row(
                    "SELECT version,stamp,value FROM lww_units WHERE key=?1", [entry.key.as_str()],
                    |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
                ).optional()?;
                if authority != header.binding_authority.0.to_string() || entry.target_authority != header.binding_authority
                    || serde_json::from_str::<UnitValue>(&value)? != entry.value
                    || unit != Some((version.clone(), current_stamp.clone(), value)) {
                    return Err(error("unpublished-version-changed"));
                }
                if version == header.request_id && current_stamp == serde_json::to_string(stamp)? {
                    continue;
                }
                if version != entry.version || current_stamp != serde_json::to_string(&entry.stamp)? {
                    return Err(error("unpublished-version-changed"));
                }
                put_unit(
                    &tx,
                    &entry.key,
                    stamp,
                    &entry.value,
                    &header.request_id,
                    Some(header.binding_authority),
                )?;
            }
            tx.commit()?;
        }
        Ok(())
    }
    pub(super) fn lww_prepared_replacement_revision(
        &self,
        staging_id: &str,
        expected_revision: Option<i64>,
    ) -> StoreResult<i64> {
        let current_authority = self.lww_binding_authority()?;
        let row: Option<(String, String, String, bool)> = self.device_store()?.connection().query_row(
            "SELECT authority,body,digest,complete FROM lww_intents WHERE request_id=?1", [staging_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).optional()?;
        let Some((authority, body, digest, complete)) = row else {
            return commit::validate_replace_commit(&self.connection, staging_id, expected_revision);
        };
        let intent: Intent = serde_json::from_str(&body)?;
        let Intent::Replacement {
            staging_id: original, base_revision, staging_digest,
            source_units: None, device_sections: None, ..
        } = intent else { return Err(error("request-id-integrity")); };
        #[cfg(test)]
        crate::persistent_store::hash_work::observe("native_replacement_receipt", body.len());
        if authority != current_authority.0.to_string() || original != staging_id
            || risunest_sync_wire::hash(body.as_bytes()) != digest || base_revision < 0
        { return Err(error("request-id-integrity")); }
        if let Some(expected) = expected_revision {
            if expected != base_revision {
                return Err(StoreError::RevisionConflict { expected, actual: base_revision });
            }
        }
        let committed: Option<(String, i64, Option<String>)> = self.connection.query_row(
            "SELECT digest,revision,activated_generation FROM lww_requests WHERE request_id=?1", [staging_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional()?;
        if let Some((committed_digest, revision, activated)) = committed {
            if committed_digest != digest || base_revision.checked_add(1) != Some(revision)
                || activated.as_deref() != Some(staging_id)
            {
                return Err(error("request-id-integrity"));
            }
            if commit::generation_state(&self.connection, staging_id)?.as_deref() == Some("staging") {
                return Err(error("replacement-stage-reused"));
            }
        } else {
            if complete { return Err(error("replacement-receipt-missing")); }
            commit::validate_replace_commit(&self.connection, staging_id, Some(base_revision))?;
            if binding_stage::catalog_digest(&self.connection, staging_id)? != staging_digest {
                return Err(error("replacement-stage-changed"));
            }
        }
        Ok(base_revision)
    }
    pub(crate) fn lww_commit_replacement(
        &mut self,
        header: &Header,
        staging_id: &str,
    ) -> StoreResult<RevisionResult> {
        self.lww_commit_replacement_inputs(header, staging_id, false, None)
    }
    /// Commits a replacement whose source units are staged for `staging_id`.
    pub(crate) fn lww_commit_staged_replacement(
        &mut self,
        header: &Header,
        staging_id: &str,
    ) -> StoreResult<RevisionResult> {
        self.lww_commit_replacement_inputs(header, staging_id, true, None)
    }
    #[cfg(test)]
    pub(crate) fn lww_commit_replacement_units(
        &mut self,
        header: &Header,
        staging_id: &str,
        source_units: Option<&BTreeMap<UnitKey, UnitValue>>,
    ) -> StoreResult<RevisionResult> {
        if let Some(units) = source_units {
            self.stage_replacement_source(staging_id, units)?;
        }
        self.lww_commit_replacement_inputs(header, staging_id, source_units.is_some(), None)
    }
    /// Commits a replacement whose source units are staged for `staging_id`, with device sections.
    pub(crate) fn lww_commit_staged_replacement_with_device_sections(
        &mut self,
        header: &Header,
        staging_id: &str,
        sections: &[&device_store::sections::PreparedSectionRows],
    ) -> StoreResult<RevisionResult> {
        let tx = self.device_store_mut()?.transaction()?;
        for section in sections { section.retain_plugin_objects(&tx)?; }
        tx.commit()?;
        let frozen = device_store::sections::freeze_backup_sections(sections)?;
        self.lww_commit_replacement_inputs(header, staging_id, true, Some(&frozen))
    }
    #[cfg(test)]
    pub(crate) fn lww_commit_replacement_with_device_sections(
        &mut self,
        header: &Header,
        staging_id: &str,
        source_units: Option<&BTreeMap<UnitKey, UnitValue>>,
        sections: &[&device_store::sections::PreparedSectionRows],
    ) -> StoreResult<RevisionResult> {
        if let Some(units) = source_units {
            self.stage_replacement_source(staging_id, units)?;
        }
        let tx = self.device_store_mut()?.transaction()?;
        for section in sections { section.retain_plugin_objects(&tx)?; }
        tx.commit()?;
        let frozen = device_store::sections::freeze_backup_sections(sections)?;
        self.lww_commit_replacement_inputs(header, staging_id, source_units.is_some(), Some(&frozen))
    }
    fn lww_commit_replacement_inputs(
        &mut self,
        header: &Header,
        staging_id: &str,
        staged_source: bool,
        device_sections: Option<&device_store::sections::FrozenBackupSections>,
    ) -> StoreResult<RevisionResult> {
        validate_header(header)?;
        if staged_source && replacement_source::contains_device_unit(&self.connection, staging_id)? {
            return Err(error("replacement-library-source-contains-device-units"));
        }
        let source = staged_source.then(|| replacement_source::digest(&self.connection, staging_id)).transpose()?;
        let existing: Option<(String, String, bool)> = self
            .device_store()?
            .connection()
            .query_row(
                "SELECT authority,body,complete FROM lww_intents WHERE request_id=?1",
                [&header.request_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some((authority, body, complete)) = existing {
            if authority != header.binding_authority.0.to_string() {
                return Err(error("request-id-integrity"));
            }
            let intent: Intent = serde_json::from_str(&body)?;
            if !matches!(&intent,Intent::Replacement{staging_id:old,source_units:old_units,device_sections:old_sections,..} if old==staging_id&&*old_units==source&&old_sections.as_deref()==device_sections)
            {
                return Err(error("request-id-integrity"));
            }
            if !complete {
                self.lww_recover_intents()?;
            }
            return self
                .completed_intent(header, &intent)?
                .ok_or_else(|| error("replacement-intent-incomplete"));
        }
        self.lww_recover_intents()?;
        verify(self.device_store()?.connection(), header.binding_authority)?;
        if binding_stage::is_binding_stage(&self.connection, staging_id)? {
            return Err(error("binding-stage-not-replacement"));
        }
        let base_revision = super::current_revision(&self.connection)?;
        let layer = self.remap_retired_staging(staging_id, staged_source)?;
        let effective_source = layer.map(|layer| SourceLayer { generation: staging_id, layer });
        if let Some(source) = effective_source {
            for unit in replacement_source::units(&self.connection, source) {
                let (key, value) = unit?;
                wire({
                    let result = value.validate();
                    #[cfg(test)]
                    crate::persistent_store::hash_work::validation(&value);
                    result
                })?;
                projection::validate_received(&self.connection, &key, &value)?;
            }
        }
        let generation = active_generation(&self.connection)?;
        let tx = self.connection.transaction()?;
        let changes = replacement_changes(&tx, &self.repository_root, &generation, staging_id, effective_source)?;
        tx.commit()?;
        let (device_changes, device_settings_digest) = if let Some(sections) = device_sections {
            let tx = self.device_store_mut()?.transaction()?;
            verify(&tx, header.binding_authority)?;
            let changes = projection::device_replacement_changes(&tx, sections)?;
            let settings_digest = device_store::sections::replacement_local_settings_digest(&tx)?;
            tx.commit()?;
            (changes, Some(settings_digest))
        } else { (vec![], None) };
        let staging_digest = binding_stage::catalog_digest(&self.connection, staging_id)?;
        let rows = intent_rows::write(
            &mut self.device_store_mut()?.connection,
            &header.request_id,
            "changes",
            changes.texts(),
        )?;
        let (stamp, digest) = self.reserve_intent(
            header,
            &Intent::Replacement {
                device_revision: device_revision(self.device_store()?.connection())?,
                staging_id: staging_id.into(),
                base_revision,
                staging_digest,
                changes: rows,
                source_units: source,
                device_sections: device_sections.map(Cow::Borrowed),
                device_settings_digest,
                device_changes: Cow::Borrowed(&device_changes),
            },
        )?;
        let result = if let Some(sections) = device_sections {
            self.finish_lww_device_replacement(header, staging_id, &changes, sections, &device_changes, &stamp, &digest)?
        } else {
            let result = self.finish_lww_replacement(header, staging_id, &changes, &stamp, &digest, false)?;
            self.complete_intent(header, true)?;
            result
        };
        Ok(result)
    }
    fn finish_lww_device_replacement(
        &mut self,
        header: &Header,
        staging_id: &str,
        changes: &FrozenRows,
        sections: &device_store::sections::FrozenBackupSections,
        device_changes: &[(UnitKey, UnitValue)],
        stamp: &Stamp,
        digest: &str,
    ) -> StoreResult<RevisionResult> {
        self.verify_intent_device_revision(&header.request_id)?;
        verify(self.device_store()?.connection(), header.binding_authority)?;
        let result = self.finish_lww_replacement(header, staging_id, changes, stamp, digest, false)?;
        let tx = self.device_store_mut()?.transaction()?;
        verify(&tx, header.binding_authority)?;
        verify_device_intent_revision(&tx, &header.request_id)?;
        device_store::begin_mutation_remote(&tx)?;
        device_store::sections::restore_frozen_backup_sections(&tx, sections, stamp)?;
        for (key, value) in device_changes {
            put_unit(&tx, key, stamp, value, &header.request_id, Some(header.binding_authority))?;
        }
        device_store::finish_mutation_remote(&tx)?;
        tx.execute("UPDATE lww_intents SET complete=1 WHERE request_id=?1", [&header.request_id])?;
        tx.execute("DELETE FROM lww_intent_rows WHERE request_id=?1", [&header.request_id])?;
        tx.execute("DELETE FROM lww_intent_failures WHERE request_id=?1", [&header.request_id])?;
        tx.execute("DELETE FROM lww_intent_proofs WHERE request_id=?1", [&header.request_id])?;
        tx.commit()?;
        Ok(result)
    }
    pub(crate) fn lww_replace_target(
        &mut self,
        header: &Header,
        staging_id: &str,
        changes: &[Change],
    ) -> StoreResult<RevisionResult> {
        let rows = FrozenRows::new(&self.repository_root)?;
        for change in changes { rows.insert(intent_rows::target_row(change)?)?; }
        self.lww_replace_target_rows(header, staging_id, &rows)
    }
    pub(super) fn lww_replace_target_rows(
        &mut self, header: &Header, staging_id: &str, changes: &FrozenRows,
    ) -> StoreResult<RevisionResult> {
        let issued: bool = self.device_store()?.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM lww_intents WHERE request_id=?1)", [&header.request_id], |row| row.get(0),
        )?;
        if issued {
            // Only a retry needs the digest of rows it does not store again.
            let intent = Intent::Target {
                device_revision: self.intent_device_revision(&header.request_id)?,
                staging_id: staging_id.into(),
                changes: intent_rows::digest("target", changes.texts())?,
            };
            if let Some(result) = self.completed_intent(header, &intent)? {
                return Ok(result);
            }
            self.lww_recover_intents()?;
            if let Some(result) = self.completed_intent(header, &intent)? {
                return Ok(result);
            }
        } else {
            self.lww_recover_intents()?;
        }
        verify(self.device_store()?.connection(), header.binding_authority)?;
        changes.visit(false, |key, stamp, value, _| {
            wire(value.validate())?;
            wire(stamp.ok_or_else(|| error("request-id-integrity"))?.validate())?;
            projection::validate_received(&self.connection, &key, &value)
        })?;
        binding_stage::validate_binding_source_rows(&self.connection, &self.repository_root, staging_id, header, changes)?;
        let rows = intent_rows::write(
            &mut self.device_store_mut()?.connection,
            &header.request_id,
            "target",
            changes.texts(),
        )?;
        let intent = Intent::Target { device_revision: device_revision(self.device_store()?.connection())?, staging_id: staging_id.into(), changes: rows };
        let (stamp, digest) = self.reserve_intent(header, &intent)?;
        let result = self.finish_lww_replacement(
            header, staging_id, changes, &stamp, &digest, true,
        )?;
        self.complete_intent(header, intent.kept_after_completion())?;
        Ok(result)
    }
    fn finish_lww_replacement(
        &mut self,
        header: &Header,
        staging_id: &str,
        changes: &FrozenRows,
        stamp: &Stamp,
        digest: &str,
        target: bool,
    ) -> StoreResult<RevisionResult> {
        if target { self.verify_intent_device_revision(&header.request_id)?; }
        let proof = if target { None } else {
            let body: String = self.device_store()?.connection().query_row(
                "SELECT body FROM lww_intents WHERE request_id=?1", [&header.request_id], |row| row.get(0),
            )?;
            let intent: Intent = serde_json::from_str(&body)?;
            self.completed_intent(header, &intent)?;
            let Intent::Replacement { staging_id: original, base_revision, staging_digest, .. } = intent
                else { return Err(error("request-id-integrity")); };
            if original != staging_id { return Err(error("request-id-integrity")); }
            Some((base_revision, staging_digest))
        };
        let result = commit::replace_commit_lww_rows(
            &mut self.connection,
            &self.repository_root,
            staging_id,
            header,
            stamp,
            digest,
            changes,
            target,
            None,
            None,
            proof.as_ref().map(|(base_revision, staging_digest)| commit::ReplacementProof {
                base_revision: *base_revision,
                staging_digest,
            }),
        )?;
        if target {
            changes.visit(false, |key, stamp, value, _| {
                let change = Change { key, stamp: stamp.ok_or_else(|| error("request-id-integrity"))?, value };
                self.copy_device_unit_bodies(std::iter::once(&change))
            })?;
            let tx = self.device_store_mut()?.transaction()?;
            verify(&tx, header.binding_authority)?;
            verify_device_intent_revision(&tx, &header.request_id)?;
            reset_target_device_rows(&tx, header, changes, header.binding_authority)?;
            tx.execute("UPDATE lww_intents SET complete=1 WHERE request_id=?1", [&header.request_id])?;
            tx.execute("DELETE FROM lww_intent_rows WHERE request_id=?1", [&header.request_id])?;
            tx.execute("DELETE FROM lww_intent_failures WHERE request_id=?1", [&header.request_id])?;
            tx.execute("DELETE FROM lww_intent_proofs WHERE request_id=?1", [&header.request_id])?;
            tx.commit()?;
        }
        Ok(result)
    }
}

/// The character whose archive decides whether a replacement carries this key.
fn archived_character(key: &UnitKey) -> Option<String> {
    let mut p = key.components();
    match p[0].as_str() {
        "character" | "conversation" | "messages" => Some(p.swap_remove(1)),
        "exists" if p[1] == "conversation" => Some(p.swap_remove(2)),
        "order" if p[1] == "conversations" => Some(p.swap_remove(2)),
        _ => None,
    }
}

/// Capture one record at a time and spool inputs, differences, overrides and
/// membership to disk. Stored message manifests keep captures independent of
/// message bodies, and replay retains the same key order and frozen digest.
fn replacement_changes(
    tx: &Transaction<'_>,
    root: &std::path::Path,
    active: &str,
    staging_id: &str,
    source: Option<SourceLayer<'_>>,
) -> StoreResult<FrozenRows> {
    let sourced = |key: &UnitKey| -> StoreResult<Option<UnitValue>> {
        source.map(|source| replacement_source::get(tx, source, key)).transpose().map(Option::flatten)
    };
    let changes = FrozenRows::new(root)?;
    changes.db.execute_batch("CREATE TABLE captured(key TEXT PRIMARY KEY,before_value TEXT,staged_value TEXT);")?;
    for (generation, column) in [(active, "before_value"), (staging_id, "staged_value")] {
        let sql = format!("INSERT INTO captured(key,{column}) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET {column}=excluded.{column}");
        projection::visit_generation_units(tx, generation, |key, value| {
            changes.db.execute(&sql, params![key.as_str(),serde_json::to_string(&value)?])?;
            Ok(())
        })?;
    }
    let archived = |id: &str| -> StoreResult<bool> {
        Ok(match sourced(&unit_key(&["archive", id])?)? {
            Some(value) => matches!(value, UnitValue::Object { .. }),
            None => {
                let value: Option<Option<String>> = changes.db.query_row("SELECT staged_value FROM captured WHERE key=?1", [unit_key(&["archive", id])?.as_str()], |row| row.get(0)).optional()?;
                value.flatten().map(|value| serde_json::from_str::<UnitValue>(&value)).transpose()?.is_some_and(|value| matches!(value, UnitValue::Object { .. }))
            },
        })
    };
    let decide = |key: UnitKey, before: Option<&UnitValue>, staged: Option<&UnitValue>, skip: bool| -> StoreResult<()> {
        let source_value = sourced(&key)?;
        if skip && source_value.is_none() {
            return Ok(());
        }
        let value = source_value.clone().or_else(|| staged.cloned()).unwrap_or(UnitValue::Deleted);
        if !matches!(value, UnitValue::Deleted) && parent_status(tx, &key)? == "retired" {
            // The identity remap moved every record the stage holds, so a
            // source unit still under a retired ID belongs to no restored
            // record and stays covered by its tombstone.
            if staged.is_none() {
                return Ok(());
            }
            return Err(error("retired-record-id"));
        }
        let prior = read_unit(tx, &key)?
            .map(|(_, value)| value)
            .or_else(|| before.cloned())
            .unwrap_or(UnitValue::Deleted);
        if prior != value {
            changes.insert(intent_rows::replacement_row(&key, &value, source_value.is_some() && staged != source_value.as_ref())?)?;
        }
        Ok(())
    };
    let mut captured = changes.db.prepare("SELECT key,before_value,staged_value FROM captured ORDER BY key")?;
    let mut captured_rows = captured.query([])?;
    while let Some(row) = captured_rows.next()? {
        let key: UnitKey = wire(row.get::<_,String>(0)?.try_into())?;
        let before = row.get::<_,Option<String>>(1)?.map(|value| serde_json::from_str::<UnitValue>(&value)).transpose()?;
        let staged = row.get::<_,Option<String>>(2)?.map(|value| serde_json::from_str::<UnitValue>(&value)).transpose()?;
        let skip = archived_character(&key).map(|id| archived(&id)).transpose()?.unwrap_or(false);
        decide(key, before.as_ref(), staged.as_ref(), skip)?;
    }
    drop(captured_rows);
    drop(captured);
    for unit in source.into_iter().flat_map(|source| replacement_source::units(tx, source)) {
        let (key, _) = unit?;
        let captured: bool = changes.db.query_row("SELECT EXISTS(SELECT 1 FROM captured WHERE key=?1)", [key.as_str()], |row| row.get(0))?;
        if !captured {
            let skip = archived_character(&key).map(|id| archived(&id)).transpose()?.unwrap_or(false);
            decide(key, None, None, skip)?;
        }
    }
    let mut statement = tx.prepare("SELECT key FROM lww_units ORDER BY key")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let key: UnitKey = wire(row.get::<_, String>(0)?.try_into())?;
        let captured: bool = changes.db.query_row("SELECT EXISTS(SELECT 1 FROM captured WHERE key=?1)", [key.as_str()], |row| row.get(0))?;
        if projection::known(&key) && !captured && sourced(&key)?.is_none() {
            let skip = archived_character(&key).map(|id| archived(&id)).transpose()?.unwrap_or(false);
            decide(key, None, None, skip)?;
        }
    }
    Ok(changes)
}

#[cfg(test)]
impl PersistentStore {
    /// Compare streamed replacement changes with the whole-library reference.
    pub(crate) fn replacement_change_sets(
        &mut self,
        staging_id: &str,
        source: Option<&BTreeMap<UnitKey, UnitValue>>,
    ) -> StoreResult<[(Vec<(UnitKey, UnitValue)>, BTreeSet<UnitKey>); 2]> {
        if let Some(units) = source {
            self.stage_replacement_source(staging_id, units)?;
        }
        let generation = active_generation(&self.connection)?;
        let tx = self.connection.transaction()?;
        let rows = replacement_changes(&tx, &self.repository_root, &generation, staging_id, source.map(|_| SourceLayer::staged(staging_id)))?;
        let mut merged = (Vec::new(), BTreeSet::new());
        rows.visit(false, |key, _, value, source_override| {
            if source_override { merged.1.insert(key.clone()); }
            merged.0.push((key, value));
            Ok(())
        })?;
        tx.commit()?;
        let whole = tests::whole_library_changes(&mut self.connection, &generation, staging_id, source)?;
        Ok([merged, whole])
    }
}

/// Errors that show a frozen intent no longer matches its stored inputs or
/// receipt. They stop recovery instead of closing the intent.
fn breaks_intent_integrity(failure: &StoreError) -> bool {
    matches!(failure, StoreError::Validation { message } if matches!(
        message.as_str(),
        "request-id-integrity" | "request-receipt-missing" | "replacement-stage-changed" | "replacement-receipt-missing"
    ))
}

/// The revision and the generation that a replacement receipt activated.
pub(crate) fn activated_receipt(library: &Connection, request_id: &str) -> StoreResult<Option<(i64, String)>> {
    let row: Option<(i64, Option<String>)> = library.query_row(
        "SELECT revision,activated_generation FROM lww_requests WHERE request_id=?1", [request_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()?;
    Ok(row.and_then(|(revision, generation)| generation.map(|generation| (revision, generation))))
}

pub(crate) fn completed_device_replacement_receipt(
    library: &Connection,
    device: &Connection,
    header: &Header,
    staging_id: &str,
) -> StoreResult<Option<RevisionResult>> {
    validate_header(header)?;
    let row: Option<(String,String,String,bool)> = device.query_row(
        "SELECT authority,body,digest,complete FROM lww_intents WHERE request_id=?1", [&header.request_id],
        |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
    ).optional()?;
    let Some((authority, body, digest, complete)) = row else { return Ok(None); };
    let intent: Intent = serde_json::from_str(&body)?;
    let Intent::Replacement { staging_id: stage, base_revision, device_sections: Some(_), .. } = intent
        else { return Err(error("request-id-integrity")); };
    if authority != header.binding_authority.0.to_string() || stage != staging_id || base_revision < 0 {
        return Err(error("request-id-integrity"));
    }
    #[cfg(test)]
    crate::persistent_store::hash_work::observe("native_replacement_receipt", body.len());
    if risunest_sync_wire::hash(body.as_bytes()) != digest { return Err(error("request-id-integrity")); }
    if !complete { return Ok(None); }
    let committed: Option<(String,i64)> = library.query_row(
        "SELECT digest,revision FROM lww_requests WHERE request_id=?1", [&header.request_id],
        |row| Ok((row.get(0)?,row.get(1)?)),
    ).optional()?;
    let Some((committed_digest, revision)) = committed else { return Err(error("replacement-receipt-missing")); };
    if committed_digest != digest || base_revision.checked_add(1) != Some(revision) {
        return Err(error("request-id-integrity"));
    }
    Ok(Some(RevisionResult { revision }))
}

fn carry_units(db: &Connection, old: DecimalU64, new: DecimalU64) -> StoreResult<()> {
    let (old, new) = (old.0.to_string(), new.0.to_string());
    db.execute("UPDATE lww_outbox SET authority=?2 WHERE authority=?1", params![old, new])?;
    db.execute("UPDATE OR REPLACE lww_publications SET authority=?2 WHERE authority=?1", params![old, new])?;
    db.execute("UPDATE OR REPLACE lww_initialization_scopes SET authority=?2 WHERE authority=?1", params![old, new])?;
    Ok(())
}
fn unfinished_receives(db: &Connection, authority: DecimalU64) -> StoreResult<Vec<String>> {
    let mut statement = db.prepare("SELECT request_id FROM lww_receive WHERE authority=?1 AND finished=0")?;
    let ids = statement.query_map([authority.0.to_string()], |r| r.get(0))?.collect::<Result<_, _>>()?;
    Ok(ids)
}
fn carry_device_progress(db: &Connection, old: DecimalU64, new: DecimalU64) -> StoreResult<()> {
    let (old_text, new_text) = (old.0.to_string(), new.0.to_string());
    db.execute("UPDATE lww_progress SET authority=?2 WHERE authority=?1", params![old_text, new_text])?;
    db.execute("UPDATE lww_unpublished_proofs SET authority=?2 WHERE authority=?1", params![old_text, new_text])?;
    // An unfinished receive is staged again from the start under the new authority. A finished one
    // stays the proof for the rows it still holds, so its body moves with the authority.
    db.execute("DELETE FROM lww_receive_rows WHERE request_id IN (SELECT request_id FROM lww_receive WHERE authority=?1 AND finished=0)", [&old_text])?;
    db.execute("DELETE FROM lww_receive WHERE authority=?1 AND finished=0", [&old_text])?;
    let finished: Vec<(String, String)> = {
        let mut statement = db.prepare("SELECT request_id,body FROM lww_receive WHERE authority=?1")?;
        let rows = statement.query_map([&old_text], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<_, _>>()?;
        rows
    };
    for (request_id, body) in finished {
        let mut request: StageReceive = serde_json::from_str(&body)?;
        request.header.binding_authority = new;
        let body = serde_json::to_string(&request)?;
        db.execute(
            "UPDATE lww_receive SET authority=?2,digest=?3,body=?4 WHERE request_id=?1",
            params![request_id, new_text, risunest_sync_wire::hash(body.as_bytes()), body],
        )?;
    }
    Ok(())
}
#[cfg(test)]
thread_local! {
    static INITIAL_QUEUE_PAGE: std::cell::Cell<usize> = const { std::cell::Cell::new(4096) };
    static INITIAL_QUEUE_COMMITS: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    static SWITCH_LIBRARY_COMMITTED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
fn reset_target_device(
    tx: &Transaction<'_>,
    root: &std::path::Path,
    header: &Header,
    received: &[Change],
    binding: DecimalU64,
) -> StoreResult<()> {
    let rows = FrozenRows::new(root)?;
    for change in received { rows.insert(intent_rows::target_row(change)?)?; }
    reset_target_device_rows(tx, header, &rows, binding)
}
fn reset_target_device_rows(tx: &Transaction<'_>, header: &Header, received: &FrozenRows, binding: DecimalU64) -> StoreResult<()> {
    tx.execute("DELETE FROM hypa_embeddings", [])?;
    tx.execute("DELETE FROM plugin_device_storage", [])?;
    tx.execute("DELETE FROM lww_units", [])?;
    tx.execute("DELETE FROM lww_outbox", [])?;
    tx.execute("DELETE FROM lww_receive_rows", [])?;
    received.visit(false, |key, stamp, value, _| {
        let c = Change { key, stamp: stamp.ok_or_else(|| error("request-id-integrity"))?, value };
        observe(tx, &c.stamp)?;
        if projection::is_device(&c.key) {
            projection::apply(tx, "", &c.key, &c.value)?;
            put_unit(tx, &c.key, &c.stamp, &c.value, &header.request_id, None)?;
            witness_received(tx, &c, binding)?;
        }
        Ok(())
    })
}

#[cfg(test)]
#[path = "lww_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "lww_large_unit_tests.rs"]
mod large_unit_tests;

pub(super) fn validate_local_targets(db: &Connection, input: &WorkingSetCommit) -> StoreResult<()> {
    for value in input
        .character
        .iter()
        .chain(input.character_details.iter().flatten())
        .chain(input.replace_character.iter())
        .chain(input.add_character.iter())
    {
        if let Some(id) = value.get("chaId").and_then(Value::as_str) {
            if is_retired(db, &unit_key(&["exists", "character", id])?)? {
                return Err(error("retired-record-id"));
            }
        }
    }
    for value in input.root.iter() {
        for collection in ["modules", "loadouts", "customModels", "personas"] {
            for record in value
                .get(collection)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(id) = record.get("id").and_then(Value::as_str) {
                    if is_retired(
                        db,
                        &unit_key(&[
                            "exists",
                            if collection == "personas" {
                                "persona"
                            } else {
                                collection
                            },
                            id,
                        ])?,
                    )? {
                        return Err(error("retired-record-id"));
                    }
                }
            }
        }
    }
    for preset in input.replace_presets.iter().flatten() {
        if let Some(id) = preset.get("id").and_then(Value::as_str) {
            if is_retired(db, &unit_key(&["exists", "preset", id])?)? {
                return Err(error("retired-record-id"));
            }
        }
    }
    // A write under a deleted record is refused; a delete there changes nothing.
    for mutation in input.unit_mutations.iter().flatten() {
        if let UnitMutation::Set { key, .. } = mutation {
            if parent_status(db, key)? == "retired" {
                return Err(error("retired-record-id"));
            }
        }
    }
    for mutation in input.conversations.iter().flatten() {
        if let super::ConversationMutation::ReplaceRange {
            character_id,
            conversation_id,
            ..
        } = mutation
        {
            if parent_status(db, &unit_key(&["messages", character_id, conversation_id])?)?
                == "retired"
            {
                return Err(error("retired-record-id"));
            }
        }
    }
    Ok(())
}
pub(super) fn validate_schema(db: &Connection) -> StoreResult<()> {
    let reference = Connection::open_in_memory()?;
    reference.execute_batch(UNIT_SCHEMA)?;
    reference.execute_batch(BINDING_STAGE_SCHEMA)?;
    reference.execute_batch(super::message_pages::SCHEMA)?;
    let mut stmt =
        reference.prepare("SELECT type,name,sql FROM sqlite_master WHERE sql IS NOT NULL")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (kind, name, sql) = row?;
        let actual: Option<String> = db
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type=?1 AND name=?2",
                params![kind, name],
                |r| r.get(0),
            )
            .optional()?;
        let normalize = |s: &str| {
            s.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .trim_end_matches(';')
                .to_owned()
        };
        if actual.as_deref().map(normalize) != Some(normalize(&sql)) {
            return Err(StoreError::SchemaMismatch { message: "invalid-lww-schema".to_owned() });
        }
    }
    let columns: Vec<String> = {
        let mut s = db.prepare("PRAGMA table_info(messages)")?;
        let v = s.query_map([], |r| r.get(1))?.collect::<Result<_, _>>()?;
        v
    };
    if !columns.iter().any(|c| c == "canonical_hash")
        || !columns.iter().any(|c| c == "canonical_size")
    {
        return Err(StoreError::SchemaMismatch { message: "invalid-message-hash-schema".to_owned() });
    }
    Ok(())
}

pub(super) struct ArchiveContext<'a> {
    pub header: &'a Header,
    pub stamp: &'a Stamp,
    pub digest: &'a str,
    pub incoming: Option<&'a Change>,
}
pub(super) use projection::{
    archive_metadata, shared_archive_character, shared_archive_conversation,
};
pub(super) fn activate_plugin_local_units(tx: &Transaction<'_>) -> StoreResult<()> {
    let rows: Vec<(String, String)> = {
        let mut q = tx.prepare(
            "SELECT key,value FROM lww_units WHERE json_extract(key,'$[0]')='plugin-local'",
        )?;
        let rows = q
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        rows
    };
    for (key, value) in rows {
        let key = wire(key.try_into())?;
        let value = serde_json::from_str(&value)?;
        projection::apply(tx, "", &key, &value)?;
    }
    Ok(())
}
pub(super) fn archive_request_result(
    db: &Connection,
    context: Option<&ArchiveContext<'_>>,
) -> StoreResult<Option<RevisionResult>> {
    let Some(context) = context else {
        return Ok(None);
    };
    let result: Option<(String, i64)> = db
        .query_row(
            "SELECT digest,revision FROM lww_requests WHERE request_id=?1",
            [&context.header.request_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    result
        .map(|(digest, revision)| {
            if digest == context.digest {
                Ok(RevisionResult { revision })
            } else {
                Err(error("request-id-integrity"))
            }
        })
        .transpose()
}
pub(super) fn record_archive_state(
    tx: &Transaction<'_>,
    generation: &str,
    char_id: &str,
    context: Option<&ArchiveContext<'_>>,
    restore: bool,
) -> StoreResult<()> {
    let Some(context) = context else {
        return Ok(());
    };
    if restore {
        projection::reproject_archived_children(tx, generation, char_id)?;
    }
    let value = if let Some(incoming) = context.incoming {
        incoming.value.clone()
    } else if let Some(archived) = super::archive::read_archived_object(tx, generation, char_id)? {
        projection::archive_value(tx, &archived)?
    } else {
        UnitValue::Deleted
    };
    let key = unit_key(&["archive", char_id])?;
    put_unit(
        tx,
        &key,
        context.stamp,
        &value,
        &context.header.request_id,
        if context.incoming.is_some() {
            None
        } else {
            Some(context.header.binding_authority)
        },
    )?;
    if context.incoming.is_none() {
        ensure_publishable_parents(tx, &key, context.header.binding_authority)?;
    } else if let Some(incoming) = context.incoming {
        witness_received(tx, incoming, context.header.binding_authority)?;
    }
    tx.execute(
        "INSERT INTO lww_requests(request_id,digest,revision) VALUES(?1,?2,?3)",
        params![
            context.header.request_id,
            context.digest,
            current_revision(tx)? + 1
        ],
    )?;
    Ok(())
}

fn witness_exact(
    db: &Connection,
    key: &UnitKey,
    binding: DecimalU64,
    version: &str,
    stamp: &Stamp,
    identity: &str,
) -> StoreResult<()> {
    db.execute("INSERT INTO lww_publications SELECT key,?1,version,stamp,identity,json_extract(value,'$.descriptor.objectHash') FROM lww_units WHERE key=?2 AND version=?3 AND stamp=?4 AND identity=?5 ON CONFLICT(key,authority) DO UPDATE SET version=excluded.version,stamp=excluded.stamp,identity=excluded.identity,object_hash=excluded.object_hash",params![binding.0.to_string(),key.as_str(),version,serde_json::to_string(stamp)?,identity])?;
    Ok(())
}
pub(super) fn witness_received(
    db: &Connection,
    change: &Change,
    binding: DecimalU64,
) -> StoreResult<()> {
    let row: Option<(String, String)> = db
        .query_row(
            "SELECT version,identity FROM lww_units WHERE key=?1 AND stamp=?2",
            params![change.key.as_str(), serde_json::to_string(&change.stamp)?],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((version, identity)) = row {
        if identity == value_identity(&change.value)? {
            witness_exact(db, &change.key, binding, &version, &change.stamp, &identity)?;
            db.execute("DELETE FROM lww_outbox WHERE key=?1 AND version=?2 AND stamp=?3 AND identity=?4 AND authority=?5",params![change.key.as_str(),version,serde_json::to_string(&change.stamp)?,identity,binding.0.to_string()])?;
        }
    }
    Ok(())
}
fn enqueue_existing(db: &Connection, key: &UnitKey, binding: DecimalU64) -> StoreResult<()> {
    if parent_status(db, key)? == "retired" && !is_deleted_existence(db, key)? {
        return Ok(());
    }
    db.execute("INSERT INTO lww_outbox SELECT u.key,u.stamp,u.value,u.version,u.identity,?1 FROM lww_units u WHERE u.key=?2 AND NOT EXISTS(SELECT 1 FROM lww_publications p WHERE p.key=u.key AND p.authority=?1 AND p.version=u.version AND p.stamp=u.stamp AND p.identity=u.identity) ON CONFLICT(key) DO UPDATE SET stamp=excluded.stamp,value=excluded.value,version=excluded.version,identity=excluded.identity,authority=excluded.authority",params![binding.0.to_string(),key.as_str()])?;
    Ok(())
}
fn is_deleted_existence(db: &Connection, key: &UnitKey) -> StoreResult<bool> {
    Ok(key.components()[0] == "exists"
        && read_unit(db, key)?.is_some_and(|(_, value)| matches!(value, UnitValue::Deleted)))
}
fn initialize_owner(
    db: &Connection,
    kind: &str,
    first: &str,
    second: Option<&str>,
    binding: DecimalU64,
) -> StoreResult<()> {
    let parent = if kind == "conversation" {
        unit_key(&["exists", kind, first, second.unwrap()])?
    } else {
        unit_key(&["exists", kind, first])?
    };
    if read_unit(db, &parent)?.is_none() || is_retired(db, &parent)? {
        return Ok(());
    }
    enqueue_existing(db, &parent, binding)?;
    let scope = serde_json::to_string(&(kind, first, second))?;
    let initialized: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM lww_initialization_scopes WHERE scope=?1 AND authority=?2)",
        params![scope, binding.0.to_string()],
        |r| r.get(0),
    )?;
    if initialized {
        return Ok(());
    }
    let (sql,parameters):(String,Vec<String>)=match kind {
        "character"=>("SELECT key FROM lww_units WHERE json_extract(key,'$[0]') IN ('character','archive') AND json_extract(key,'$[1]')=?1 UNION ALL SELECT key FROM lww_units WHERE json_extract(key,'$[0]')='order' AND json_extract(key,'$[1]')='conversations' AND json_extract(key,'$[2]')=?1".into(),vec![first.into()]),
        "conversation"=>("SELECT key FROM lww_units WHERE json_extract(key,'$[0]') IN ('conversation','messages') AND json_extract(key,'$[1]')=?1 AND json_extract(key,'$[2]')=?2".into(),vec![first.into(),second.unwrap().into()]),
        "preset"|"persona"=>("SELECT key FROM lww_units WHERE json_extract(key,'$[0]')=?1 AND json_extract(key,'$[1]')=?2".into(),vec![kind.into(),first.into()]),
        _=>("SELECT key FROM lww_units WHERE json_extract(key,'$[0]')='record' AND json_extract(key,'$[1]')=?1 AND json_extract(key,'$[2]')=?2".into(),vec![kind.into(),first.into()]),
    };
    let keys: Vec<String> = {
        let mut query = db.prepare(&sql)?;
        let rows = query
            .query_map(rusqlite::params_from_iter(parameters.iter()), |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        rows
    };
    for key in keys {
        enqueue_existing(db, &wire(key.try_into())?, binding)?;
    }
    db.execute(
        "INSERT INTO lww_initialization_scopes VALUES(?1,?2)",
        params![scope, binding.0.to_string()],
    )?;
    Ok(())
}
pub(super) fn ensure_publishable_parents(
    db: &Connection,
    key: &UnitKey,
    binding: DecimalU64,
) -> StoreResult<()> {
    for parent in parent_keys(key)? {
        let p = parent.components();
        initialize_owner(db, &p[1], &p[2], p.get(3).map(String::as_str), binding)?;
    }
    let p = key.components();
    if p[0] == "root" && matches!(p[1].as_str(), "botPresetsId" | "selectedPersona") {
        if let Some((_, value)) = read_unit(db, key)? {
            if let Some(id) = json_value_resolved(db, &value)?.and_then(|v| v.as_str().filter(|id| !id.is_empty()).map(str::to_owned)) {
                initialize_owner(
                    db,
                    if p[1] == "botPresetsId" {
                        "preset"
                    } else {
                        "persona"
                    },
                    &id,
                    None,
                    binding,
                )?;
            }
        }
    }
    Ok(())
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UnitStatePage {
    pub entries: Vec<OutboxEntry>,
    pub after_key: Option<UnitKey>,
    pub has_more: bool,
}
impl PersistentStore {
    pub(crate) fn lww_read_unit_state(
        &self,
        binding: DecimalU64,
        after: Option<&UnitKey>,
        limit: usize,
    ) -> StoreResult<UnitStatePage> {
        verify(self.device_store()?.connection(), binding)?;
        if limit == 0 || limit > 4096 {
            return Err(error("invalid-unit-state-page-limit"));
        }
        let mut entries = Vec::new();
        let participating = projection::plugin_participates(self.device_store()?.connection())?;
        for db in [&self.connection, self.device_store()?.connection()] {
            let mut cursor = after.map(UnitKey::as_str).unwrap_or("").to_owned();
            let mut emitted = 0;
            while emitted <= limit {
                let rows: Vec<(String, String, String, String)> = {
                    let mut query=db.prepare("SELECT key,stamp,value,version FROM lww_units WHERE key>?1 AND (?3 OR json_extract(key,'$[0]')<>'plugin-local') ORDER BY key LIMIT ?2")?;
                    let rows = query
                        .query_map(params![cursor, (limit + 1) as i64, participating], |r| {
                            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                        })?
                        .collect::<Result<_, _>>()?;
                    rows
                };
                let exhausted = rows.len() < limit + 1;
                for (key, stamp, value, version) in rows {
                    cursor = key.clone();
                    let key = wire(key.try_into())?;
                    if parent_status(db, &key)? == "retired" && !is_deleted_existence(db, &key)? {
                        continue;
                    }
                    entries.push(OutboxEntry {
                        key,
                        stamp: serde_json::from_str(&stamp)?,
                        value: serde_json::from_str(&value)?,
                        version,
                        target_authority: binding,
                    });
                    emitted += 1;
                    if emitted > limit {
                        break;
                    }
                }
                if exhausted || emitted > limit {
                    break;
                }
            }
        }
        entries.sort_by(|a, b| a.key.cmp(&b.key));
        let has_more = entries.len() > limit;
        entries.truncate(limit);
        let after_key = entries.last().map(|entry| entry.key.clone());
        Ok(UnitStatePage {
            entries,
            after_key,
            has_more,
        })
    }
    pub(crate) fn lww_queue_unit_state_page(
        &mut self,
        header: &Header,
        after: Option<&UnitKey>,
        limit: usize,
    ) -> StoreResult<UnitStatePage> {
        validate_header(header)?;
        let page = self.lww_read_unit_state(header.binding_authority, after, limit)?;
        for db in [
            &mut self.connection,
            &mut self.device_store.as_mut().map_err(error)?.connection,
        ] {
            let device = db
                .path()
                .is_some_and(|p| p.ends_with(device_store::DEVICE_DATABASE_FILE));
            let tx = db.transaction()?;
            for entry in &page.entries {
                if projection::is_device(&entry.key) == device {
                    enqueue_existing(&tx, &entry.key, header.binding_authority)?;
                    if !device {
                        ensure_publishable_parents(&tx, &entry.key, header.binding_authority)?;
                    }
                }
            }
            tx.commit()?;
            #[cfg(test)]
            if INITIAL_QUEUE_COMMITS.with(|left| match left.get() {
                Some(1) => {
                    left.set(None);
                    true
                }
                Some(count) => {
                    left.set(Some(count - 1));
                    false
                }
                None => false,
            }) {
                return Err(error("initial-publication-stopped"));
            }
        }
        Ok(page)
    }
    /// Queues every unit of the library when the switch to the current target
    /// left that owed, a page at a time, and clears the obligation after the
    /// last page. Queueing skips published versions, so a stopped queue starts
    /// again from the first page.
    pub(crate) fn lww_finish_initial_publication(&mut self, header: &Header) -> StoreResult<bool> {
        validate_header(header)?;
        if self.lww_binding_authority()? != header.binding_authority {
            return Err(error("stale-binding-authority"));
        }
        let owed: Option<String> = self
            .connection
            .query_row("SELECT authority FROM lww_initial_publication WHERE singleton=1", [], |r| r.get(0))
            .optional()?;
        let Some(owed) = owed else {
            return Ok(false);
        };
        if owed != header.binding_authority.0.to_string() {
            self.connection.execute("DELETE FROM lww_initial_publication WHERE authority=?1", [owed])?;
            return Ok(false);
        }
        #[cfg(not(test))]
        let limit = 4096;
        #[cfg(test)]
        let limit = INITIAL_QUEUE_PAGE.with(std::cell::Cell::get);
        let mut after = None;
        loop {
            let page = self.lww_queue_unit_state_page(header, after.as_ref(), limit)?;
            if !page.has_more {
                break;
            }
            after = page.after_key;
        }
        self.connection.execute("DELETE FROM lww_initial_publication WHERE authority=?1", [owed])?;
        Ok(true)
    }
    #[cfg(test)]
    pub(crate) fn stop_initial_queue_after_commits(&self, page: usize, commits: usize) {
        INITIAL_QUEUE_PAGE.with(|limit| limit.set(page));
        INITIAL_QUEUE_COMMITS.with(|left| left.set(Some(commits)));
    }
    #[cfg(test)]
    pub(crate) fn lww_owed_initial_publication(&self) -> StoreResult<Option<String>> {
        Ok(self
            .connection
            .query_row("SELECT authority FROM lww_initial_publication WHERE singleton=1", [], |r| r.get(0))
            .optional()?)
    }
}

#[cfg(test)]
mod hash_work_tests {
    use super::*;
    use crate::persistent_store::hash_work::{reset_hash_work, take_hash_work, DomainWork};

    #[test]
    fn envelope_bytes_alias_work_metrics_and_descriptor_validation_is_separate() {
        let value = UnitValue::object(risunest_sync_wire::descriptor::RecordDescriptor::content("a".repeat(64))).unwrap();
        let envelope = risunest_sync_wire::canonical::encode(&value).unwrap().len() as u64;
        let UnitValue::Object { descriptor, .. } = &value else { unreachable!() };
        let descriptor_bytes = descriptor.bytes().unwrap().len() as u64;
        reset_hash_work(); reset_work_metrics();
        value_identity(&value).unwrap();
        let work = take_hash_work();
        assert_eq!(work.domains["native_unit_envelope"], DomainWork { calls: 1, bytes: envelope });
        assert_eq!(work.domains["native_wire_descriptor_validation"], DomainWork { calls: 1, bytes: descriptor_bytes });
        assert!(!work.domains.contains_key("native_wire_identity"));
        assert!(work.incomplete.is_empty());
        assert_eq!(take_work_metrics(), (0, envelope));
        reset_hash_work();
        value_identity(&UnitValue::Deleted).unwrap();
        assert!(!take_hash_work().domains.contains_key("native_wire_descriptor_validation"));
    }

    #[test]
    fn routine_commit_does_not_hash_binding_proofs() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let key = UnitKey::new(&["toggle", "toggle_synthetic-counter"]).unwrap();
        for value in [true, false] {
            reset_hash_work();
            store.commit(&crate::persistent_store::WorkingSetCommit {
                expected_revision: store.revision().unwrap(),
                unit_mutations: Some(vec![UnitMutation::Set { key: key.clone(), value: serde_json::json!(value) }]),
                ..Default::default()
            }).unwrap();
            let work = take_hash_work();
            assert!(work.incomplete.is_empty());
            assert!(work.domains["native_intent"].calls > 0);
            for domain in ["binding_source_proof", "binding_status_proof", "binding_catalog_proof", "binding_stage_proof"] {
                assert!(!work.domains.contains_key(domain));
            }
        }
    }

    #[test]
    fn equal_stamp_conflict_counts_both_executed_comparison_identities() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let key = UnitKey::new(&["future", "counter"]).unwrap();
        let stamp = Stamp { physical_ms: 7.into(), logical: 0, writer_id: "00000000-0000-4000-8000-000000000001".into() };
        let local = inline(&serde_json::json!("local")).unwrap();
        let remote = inline(&serde_json::json!("remote")).unwrap();
        put_unit(&store.connection, &key, &stamp, &local, "prior", None).unwrap();
        let bytes = [local.clone(),remote.clone()].iter().map(|value| risunest_sync_wire::canonical::encode(value).unwrap().len() as u64).sum();
        reset_hash_work();
        let request = StageReceive { header: Header { binding_authority: store.lww_binding_authority().unwrap(), request_id: uuid::Uuid::new_v4().to_string() },
            changes: vec![Change { key, stamp, value: remote }], progress: Progress { kind: "server".into(), cursor: 1.into(), writer_id: None }, admitted_time_upper_ms: 10.into() };
        assert!(store.lww_stage_receive(&request).is_err());
        let work = take_hash_work();
        assert_eq!(work.domains["native_wire_identity"], DomainWork { calls: 2, bytes });
        assert!(work.incomplete.is_empty());
        assert!(!work.domains.contains_key("native_receive_intent"));
    }

    #[test]
    fn receive_input_capture_matches_actual_hash_and_persisted_body_on_exact_retry() {
        use crate::persistent_store::hash_work::{reset_receive_intent_inputs, take_receive_intent_inputs};
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let request = StageReceive {
            header: Header { binding_authority: store.lww_binding_authority().unwrap(), request_id: uuid::Uuid::new_v4().to_string() },
            changes: vec![Change { key: UnitKey::new(&["future", "synthetic-input"]).unwrap(),
                stamp: Stamp { physical_ms: 7.into(), logical: 0, writer_id: "00000000-0000-4000-8000-000000000001".into() },
                value: inline(&serde_json::json!({"escaped":"synthetic\nvalue"})).unwrap() }],
            progress: Progress { kind: "server".into(), cursor: 999.into(), writer_id: None },
            admitted_time_upper_ms: 10.into(),
        };
        let expected = serde_json::to_vec(&request).unwrap();
        reset_hash_work(); reset_receive_intent_inputs();
        store.lww_stage_receive(&request).unwrap(); store.lww_stage_receive(&request).unwrap();
        let work = take_hash_work(); let inputs = take_receive_intent_inputs();
        assert_eq!(inputs, vec![expected.clone(), expected]);
        assert_eq!(work.domains["native_receive_intent"], DomainWork { calls: inputs.len() as u64,
            bytes: inputs.iter().map(|input| input.len() as u64).sum() });
        let persisted: String = store.device_store().unwrap().connection().query_row(
            "SELECT body FROM lww_receive WHERE request_id=?1", [&request.header.request_id], |row| row.get(0)).unwrap();
        assert_eq!(persisted.as_bytes(), inputs[0]);
        store.lww_stage_receive(&request).unwrap(); assert!(take_receive_intent_inputs().is_empty());
        let mut rejected = request; rejected.admitted_time_upper_ms = 6.into();
        reset_hash_work(); reset_receive_intent_inputs();
        assert!(store.lww_stage_receive(&rejected).is_err());
        assert!(take_receive_intent_inputs().is_empty());
        assert!(!take_hash_work().domains.contains_key("native_receive_intent"));
    }
}

#[cfg(test)]
#[path = "lww_small_field_tests.rs"]
mod small_field_tests;
