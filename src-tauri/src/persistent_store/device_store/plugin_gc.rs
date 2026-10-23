use super::{invalid, DeviceStore, StoreError, StoreResult};
use risunest_sync_wire::unit::{UnitKey, UnitValue};
use rusqlite::{Connection, OpenFlags};
use std::{path::PathBuf, time::Duration};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PluginGcFence {
    instance_id: String,
    revision: i64,
}

#[must_use = "Keep the device barrier alive through the destructive library page"]
pub(crate) struct PluginGcBarrier {
    connection: Connection,
}

impl Drop for PluginGcBarrier {
    fn drop(&mut self) {
        let _ = self.connection.execute_batch("ROLLBACK");
    }
}

pub(super) fn create_triggers(db: &Connection) -> StoreResult<()> {
    for (table, event, condition) in [
        ("plugin_device_storage", "INSERT", ""),
        ("plugin_device_storage", "UPDATE", ""),
        ("plugin_device_storage", "DELETE", ""),
        ("lww_units", "INSERT", "WHEN json_extract(NEW.key,'$[0]')='plugin-local'"),
        ("lww_units", "UPDATE", "WHEN json_extract(OLD.key,'$[0]')='plugin-local' OR json_extract(NEW.key,'$[0]')='plugin-local'"),
        ("lww_units", "DELETE", "WHEN json_extract(OLD.key,'$[0]')='plugin-local'"),
    ] {
        db.execute_batch(&format!(
            "CREATE TRIGGER plugin_gc_{table}_{} AFTER {event} ON {table} {condition}
             BEGIN
             UPDATE plugin_gc_state SET revision=CASE WHEN revision=9223372036854775807 THEN RAISE(ABORT,'plugin-gc-revision-overflow') ELSE revision+1 END WHERE singleton=1;
             SELECT CASE WHEN changes()<>1 THEN RAISE(ABORT,'plugin-gc-state-missing') END;
             END", event.to_lowercase(),
        ))?;
    }
    Ok(())
}

pub(super) fn validate_state(db: &Connection) -> StoreResult<()> {
    read_fence(db).map(|_| ())
}

fn read_fence(db: &Connection) -> StoreResult<PluginGcFence> {
    let (instance_id, revision): (String, i64) = db.query_row(
        "SELECT instance_id,revision FROM plugin_gc_state WHERE singleton=1", [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if revision < 0 || uuid::Uuid::parse_str(&instance_id).ok().map(|id| id.to_string()).as_deref() != Some(&instance_id) {
        return Err(invalid("Plugin GC state is invalid"));
    }
    Ok(PluginGcFence { instance_id, revision })
}

impl DeviceStore {
    fn plugin_gc_path(&self) -> StoreResult<PathBuf> {
        self.connection.path().filter(|path| !path.is_empty()).map(PathBuf::from)
            .ok_or_else(|| invalid("Plugin GC device path is unavailable"))
    }

    pub(crate) fn visit_plugin_gc_values(
        &self,
        mut visit: impl FnMut(&str, &str, &str, &mut rusqlite::blob::Blob<'_>) -> StoreResult<()>,
        mut visit_unit: impl FnMut(&UnitKey, &UnitValue) -> StoreResult<()>,
    ) -> StoreResult<PluginGcFence> {
        let snapshot = crate::sqlite_open::open_with_flags(self.plugin_gc_path()?, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        snapshot.execute_batch("PRAGMA query_only=ON; PRAGMA mmap_size=0; BEGIN")?;
        let fence = read_fence(&snapshot)?;
        {
            let mut statement = snapshot.prepare("SELECT owner,space,key,rowid,typeof(value) FROM plugin_device_storage WHERE tombstone=0 ORDER BY owner,space,key")?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let (owner, space, key, rowid, kind): (String, String, String, i64, String) = (row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?);
                if kind != "text" && kind != "blob" { return Err(invalid("Plugin GC value is not text")); }
                let mut value = snapshot.blob_open(rusqlite::MAIN_DB, "plugin_device_storage", "value", rowid, true)?;
                visit(&owner, &space, &key, &mut value)?;
            }
        }
        {
            let mut statement = snapshot.prepare("SELECT key,value FROM lww_units WHERE json_extract(key,'$[0]')='plugin-local' ORDER BY key")?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let encoded_key: String = row.get(0)?;
                let key = UnitKey::try_from(encoded_key).map_err(|error| invalid(&error.to_string()))?;
                let value: UnitValue = serde_json::from_str(&row.get::<_, String>(1)?)?;
                let result = value.validate();
                #[cfg(test)]
                crate::persistent_store::hash_work::validation(&value);
                result.map_err(|error| invalid(&error.to_string()))?;
                visit_unit(&key, &value)?;
            }
        }
        snapshot.execute_batch("COMMIT")?;
        Ok(fence)
    }

    pub(crate) fn plugin_gc_fence_is_current(&self, fence: &PluginGcFence) -> StoreResult<bool> {
        let db = crate::sqlite_open::open_with_flags(self.plugin_gc_path()?, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        Ok(read_fence(&db)? == *fence)
    }

    pub(crate) fn acquire_plugin_gc_barrier(&self, fence: &PluginGcFence) -> StoreResult<PluginGcBarrier> {
        let db = crate::sqlite_open::open_with_flags(self.plugin_gc_path()?, OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        db.busy_timeout(Duration::ZERO)?;
        db.execute_batch("BEGIN IMMEDIATE").map_err(|error| match error {
            rusqlite::Error::SqliteFailure(ref code, _) if matches!(code.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => StoreError::CommitBusy,
            error => error.into(),
        })?;
        if read_fence(&db)? != *fence { return Err(StoreError::CommitBusy); }
        Ok(PluginGcBarrier { connection: db })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn open() -> (tempfile::TempDir, DeviceStore) {
        let directory = tempfile::tempdir().unwrap();
        let store = DeviceStore::open(directory.path()).unwrap();
        (directory, store)
    }
    fn insert(db: &Connection, space: &str, key: &str, value: &str) -> StoreResult<()> {
        db.execute("INSERT INTO plugin_device_storage(owner,space,key,value,byte_size,tombstone,write_clock,writer_id) VALUES('orphan',?1,?2,?3,?4,0,'0','')", params![space,key,value,value.len() as i64])?;
        Ok(())
    }
    fn insert_unit(db: &Connection, key: &UnitKey, value: &UnitValue) -> StoreResult<()> {
        db.execute("INSERT INTO lww_units(key,stamp,value,version,identity) VALUES(?1,'synthetic-stamp',?2,'synthetic-version',?3)",
            params![key.as_str(),serde_json::to_string(value)?,value.identity().unwrap()])?;
        Ok(())
    }
    fn collect(store: &DeviceStore) -> (PluginGcFence, Vec<(String,String,String,String)>) {
        let mut rows = Vec::new();
        let fence = store.visit_plugin_gc_values(|owner,space,key,value| {
            let mut text = String::new(); std::io::Read::read_to_string(value, &mut text)?;
            rows.push((owner.into(),space.into(),key.into(),text)); Ok(())
        }, |_,_| Ok(())).unwrap();
        (fence,rows)
    }

    #[test]
    fn generic_writes_in_both_spaces_advance_durable_revision_without_hlc_or_change_context() {
        let (_dir,store) = open();
        let initial = collect(&store).0;
        assert_eq!(initial.revision,0);
        for space in ["string","json"] {
            insert(store.connection(),space,"key","before").unwrap();
            store.connection().execute("UPDATE plugin_device_storage SET value='after' WHERE space=?1",[space]).unwrap();
            store.connection().execute("DELETE FROM plugin_device_storage WHERE space=?1",[space]).unwrap();
        }
        assert_eq!(collect(&store).0.revision,6);
        assert!(!store.plugin_gc_fence_is_current(&initial).unwrap());
        assert_eq!(store.revision().unwrap(),0);
        assert_eq!(store.connection().query_row("SELECT issued FROM lww_clock WHERE singleton=1",[],|row|row.get::<_,Option<String>>(0)).unwrap(),None);
        assert_eq!(store.connection().query_row("SELECT count(*) FROM device_changes",[],|row|row.get::<_,i64>(0)).unwrap(),0);
    }

    #[test]
    fn readonly_roots_include_both_disabled_orphan_namespaces_and_never_spool_hypa() {
        let (_dir,store) = open();
        assert!(!store.connection().query_row("SELECT participating FROM device_sections WHERE section='local-plugins'",[],|row|row.get::<_,bool>(0)).unwrap());
        insert(store.connection(),"string","string-key","assets/synthetic-string.png").unwrap();
        insert(store.connection(),"json","json-key",r#"{"asset":"assets/synthetic-json.png"}"#).unwrap();
        store.connection().execute_batch("INSERT INTO plugin_device_storage(owner,space,key,value,byte_size,tombstone,write_clock,writer_id) VALUES('orphan','string','removed',NULL,0,1,'0',''); INSERT INTO hypa_embeddings(cache_key,producer,model,preprocess_version,dimensions,vector,tombstone,write_clock,writer_id) VALUES('invalid-hypa-key','synthetic','model',1,1,x'FF',0,'0','');").unwrap();
        let before = store.revision().unwrap();
        let (fence,rows) = collect(&store);
        assert_eq!(rows,vec![
            ("orphan".into(),"json".into(),"json-key".into(),r#"{"asset":"assets/synthetic-json.png"}"#.into()),
            ("orphan".into(),"string".into(),"string-key".into(),"assets/synthetic-string.png".into()),
        ]);
        assert!(store.plugin_gc_fence_is_current(&fence).unwrap());
        assert_eq!(store.revision().unwrap(),before);
    }

    #[test]
    fn snapshot_is_fixed_while_second_connection_changes_values_and_fence_rejects_before_deletion() {
        let (dir,store) = open();
        insert(store.connection(),"string","a","before-a").unwrap();
        insert(store.connection(),"string","b","before-b").unwrap();
        let writer = Connection::open(dir.path().join(super::super::DEVICE_DATABASE_FILE)).unwrap();
        let mut values=Vec::new(); let mut changed=false;
        let fence = store.visit_plugin_gc_values(|_,_,key,value| {
            let mut text = String::new(); std::io::Read::read_to_string(value, &mut text)?;
            values.push((key.to_owned(),text));
            if !changed {
                writer.execute("UPDATE plugin_device_storage SET value='after-b' WHERE key='b'",[])?;
                insert(&writer,"json","c","after-c")?;
                changed=true;
            }
            Ok(())
        }, |_,_| Ok(())).unwrap();
        assert_eq!(values,vec![("a".into(),"before-a".into()),("b".into(),"before-b".into())]);
        assert!(!store.plugin_gc_fence_is_current(&fence).unwrap());
        assert!(matches!(store.acquire_plugin_gc_barrier(&fence),Err(StoreError::CommitBusy)));
    }

    #[test]
    fn barrier_excludes_generic_own_and_second_connection_writes_until_page_finishes() {
        let (dir,store) = open();
        insert(store.connection(),"string","a","before").unwrap();
        let fence=collect(&store).0;
        store.connection().busy_timeout(Duration::ZERO).unwrap();
        let writer=Connection::open(dir.path().join(super::super::DEVICE_DATABASE_FILE)).unwrap();
        writer.busy_timeout(Duration::ZERO).unwrap();
        let barrier=store.acquire_plugin_gc_barrier(&fence).unwrap();
        assert!(store.connection().execute("UPDATE plugin_device_storage SET value='own' WHERE key='a'",[]).is_err());
        assert!(insert(&writer,"json","b","second").is_err());
        assert!(store.plugin_gc_fence_is_current(&fence).unwrap());
        drop(barrier);
        insert(&writer,"json","b","second").unwrap();
        assert!(!store.plugin_gc_fence_is_current(&fence).unwrap());
        assert!(matches!(store.acquire_plugin_gc_barrier(&fence),Err(StoreError::CommitBusy)));
    }

    #[test]
    fn rollback_does_not_advance_revision_and_busy_writer_cannot_be_fenced() {
        let (_dir,store) = open(); let fence=collect(&store).0;
        store.connection().execute_batch("BEGIN IMMEDIATE").unwrap();
        insert(store.connection(),"string","rolled-back","temporary").unwrap();
        assert_eq!(read_fence(store.connection()).unwrap().revision,1);
        assert!(store.plugin_gc_fence_is_current(&fence).unwrap());
        assert!(matches!(store.acquire_plugin_gc_barrier(&fence),Err(StoreError::CommitBusy)));
        store.connection().execute_batch("ROLLBACK").unwrap();
        assert!(store.plugin_gc_fence_is_current(&fence).unwrap());
        assert!(collect(&store).1.is_empty());
    }

    #[test]
    fn callback_failure_releases_snapshot_and_null_live_value_is_not_silently_ignored() {
        let (_dir,store) = open(); insert(store.connection(),"string","a","before").unwrap();
        assert!(store.visit_plugin_gc_values(|_,_,_,_|Err(invalid("synthetic-observer-failure")), |_,_| Ok(())).is_err());
        let fence=collect(&store).0; let barrier=store.acquire_plugin_gc_barrier(&fence).unwrap(); drop(barrier);
        store.connection().execute("UPDATE plugin_device_storage SET value=NULL WHERE key='a'",[]).unwrap();
        assert!(store.visit_plugin_gc_values(|_,_,_,_|Ok(()), |_,_| Ok(())).is_err());
    }

    #[test]
    fn fresh_instance_rejects_old_fence_and_missing_state_or_trigger_is_not_repaired() {
        let (dir,store) = open(); let fence=collect(&store).0;
        drop(store); let store=DeviceStore::open(dir.path()).unwrap();
        assert!(store.plugin_gc_fence_is_current(&fence).unwrap());
        drop(store); std::fs::remove_file(dir.path().join(super::super::DEVICE_DATABASE_FILE)).unwrap();
        let store=DeviceStore::open(dir.path()).unwrap();
        assert_eq!(collect(&store).0.revision,fence.revision);
        assert!(!store.plugin_gc_fence_is_current(&fence).unwrap());
        assert!(matches!(store.acquire_plugin_gc_barrier(&fence),Err(StoreError::CommitBusy)));
        store.connection().execute("DELETE FROM plugin_gc_state",[]).unwrap();
        assert!(store.visit_plugin_gc_values(|_,_,_,_|Ok(()), |_,_| Ok(())).is_err());
        assert!(insert(store.connection(),"string","cannot-write","value").is_err());
        drop(store); assert!(DeviceStore::open(dir.path()).is_err());
        let (other,store)=open();
        store.connection().execute_batch("DROP TRIGGER plugin_gc_plugin_device_storage_update").unwrap();
        drop(store); assert!(DeviceStore::open(other.path()).is_err());
    }

    #[test]
    fn revision_overflow_aborts_value_write_and_preserves_exact_fence() {
        let (_dir,store)=open();
        store.connection().execute("UPDATE plugin_gc_state SET revision=?1",[i64::MAX]).unwrap();
        let fence=collect(&store).0;
        assert!(insert(store.connection(),"string","overflow","value").is_err());
        assert!(store.plugin_gc_fence_is_current(&fence).unwrap()); assert!(collect(&store).1.is_empty());
        assert_eq!(fence.revision,i64::MAX);
    }

    #[test]
    fn generic_local_unit_writes_and_key_moves_invalidate_fence_but_hypa_does_not() {
        let (_dir,store)=open(); let initial=collect(&store).0;
        let local=UnitKey::new(&["plugin-local","orphan","string","unit"]).unwrap();
        let unrelated=UnitKey::new(&["hypa","synthetic"]).unwrap();
        let value=UnitValue::inline(br#""assets/synthetic.png""#).unwrap();
        insert_unit(store.connection(),&unrelated,&value).unwrap();
        assert!(store.plugin_gc_fence_is_current(&initial).unwrap());
        insert_unit(store.connection(),&local,&value).unwrap();
        store.connection().execute("UPDATE lww_units SET version='changed' WHERE key=?1",[local.as_str()]).unwrap();
        store.connection().execute("DELETE FROM lww_units WHERE key=?1",[local.as_str()]).unwrap();
        assert_eq!(collect(&store).0.revision,3);
        store.connection().execute("UPDATE lww_units SET key=?1 WHERE key=?2",params![local.as_str(),unrelated.as_str()]).unwrap();
        store.connection().execute("UPDATE lww_units SET key=?1 WHERE key=?2",params![unrelated.as_str(),local.as_str()]).unwrap();
        assert_eq!(collect(&store).0.revision,5);
        assert!(!store.plugin_gc_fence_is_current(&initial).unwrap());
        assert_eq!(store.connection().query_row("SELECT issued FROM lww_clock WHERE singleton=1",[],|r|r.get::<_,Option<String>>(0)).unwrap(),None);
    }

    #[test]
    fn native_and_unit_callbacks_share_snapshot_and_barrier_excludes_unit_writers() {
        let (dir,store)=open();
        insert(store.connection(),"string","native","before-native").unwrap();
        let key=UnitKey::new(&["plugin-local","orphan","json","held"]).unwrap();
        let before=UnitValue::inline(br#"{"asset":"assets/before.png"}"#).unwrap();
        let after=UnitValue::inline(br#"{"asset":"assets/after.png"}"#).unwrap();
        insert_unit(store.connection(),&key,&before).unwrap();
        let writer=Connection::open(dir.path().join(super::super::DEVICE_DATABASE_FILE)).unwrap();
        writer.busy_timeout(Duration::ZERO).unwrap();
        let mut units=Vec::new();
        let fence=store.visit_plugin_gc_values(|_,_,_,_| {
            writer.execute("UPDATE lww_units SET value=?1 WHERE key=?2",params![serde_json::to_string(&after)?,key.as_str()])?;
            Ok(())
        },|key,value| { units.push((key.clone(),value.clone())); Ok(()) }).unwrap();
        assert_eq!(units,vec![(key.clone(),before)]);
        assert!(!store.plugin_gc_fence_is_current(&fence).unwrap());
        let current=collect(&store).0;
        let barrier=store.acquire_plugin_gc_barrier(&current).unwrap();
        store.connection().busy_timeout(Duration::ZERO).unwrap();
        assert!(store.connection().execute("DELETE FROM lww_units WHERE key=?1",[key.as_str()]).is_err());
        assert!(writer.execute("UPDATE lww_units SET version='changed' WHERE key=?1",[key.as_str()]).is_err());
        drop(barrier);
        writer.execute("DELETE FROM lww_units WHERE key=?1",[key.as_str()]).unwrap();
        assert!(!store.plugin_gc_fence_is_current(&current).unwrap());
    }

    #[test]
    fn received_disabled_values_in_both_namespaces_are_typed_roots_without_active_rows() {
        use crate::persistent_store::{lww::{ApplyReceive,Change,Header,Progress,StageReceive}, PersistentStore};
        use risunest_sync_wire::stamp::Stamp;
        let dir=tempfile::tempdir().unwrap(); let mut store=PersistentStore::open(dir.path()).unwrap();
        let stamp=Stamp { physical_ms:1.into(),logical:0,writer_id:uuid::Uuid::new_v4().to_string() };
        let changes=vec![
            Change { key:UnitKey::new(&["plugin-local","orphan","string","received"]).unwrap(),stamp:stamp.clone(),value:UnitValue::inline(br#""assets/string.png""#).unwrap() },
            Change { key:UnitKey::new(&["plugin-local","orphan","json","received"]).unwrap(),stamp,value:UnitValue::inline(br#"{"asset":"assets/json.png"}"#).unwrap() },
        ];
        let header=Header { binding_authority:store.lww_binding_authority().unwrap(),request_id:uuid::Uuid::new_v4().to_string() };
        store.lww_stage_receive(&StageReceive { header:header.clone(),changes:changes.clone(),progress:Progress { kind:"server".into(),cursor:1.into(),writer_id:None },admitted_time_upper_ms:2.into() }).unwrap();
        store.lww_apply_receive(&ApplyReceive { header:header.clone(),generating:vec![] }).unwrap();
        store.lww_finish_receive(&header).unwrap();
        let device=store.device_store().unwrap();
        assert!(device.list_plugin_device_storage().unwrap().is_empty());
        let clock=store.lww_clock_state().unwrap();
        let mut roots=Vec::new();
        let fence=device.visit_plugin_gc_values(|_,_,_,_|panic!("disabled received values must remain unprojected"),|key,value| {
            use base64::Engine;
            let UnitValue::Inline { bytes }=value else { panic!("synthetic inline expected") };
            roots.push((key.clone(),serde_json::from_slice::<serde_json::Value>(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(bytes).unwrap()).unwrap()));
            Ok(())
        }).unwrap();
        assert_eq!(roots,vec![
            (changes[1].key.clone(),serde_json::json!({"asset":"assets/json.png"})),
            (changes[0].key.clone(),serde_json::json!("assets/string.png")),
        ]);
        assert!(device.plugin_gc_fence_is_current(&fence).unwrap());
        assert_eq!(store.lww_clock_state().unwrap().issued,clock.issued);
        assert_eq!(store.lww_clock_state().unwrap().accepted,clock.accepted);
    }

    #[test]
    fn unit_visitor_passes_object_and_deleted_values_without_body_io_and_rejects_corruption() {
        let (_dir,store)=open();
        let key=UnitKey::new(&["plugin-local","orphan","json","object"]).unwrap();
        let object=UnitValue::object(risunest_sync_wire::descriptor::RecordDescriptor::content("a".repeat(64))).unwrap();
        insert_unit(store.connection(),&key,&object).unwrap();
        let deleted=UnitKey::new(&["plugin-local","orphan","string","deleted"]).unwrap();
        insert_unit(store.connection(),&deleted,&UnitValue::Deleted).unwrap();
        let mut units=Vec::new();
        store.visit_plugin_gc_values(|_,_,_,_|Ok(()),|key,value| { units.push((key.clone(),value.clone())); Ok(()) }).unwrap();
        assert_eq!(units,vec![(key.clone(),object),(deleted.clone(),UnitValue::Deleted)]);
        assert!(store.visit_plugin_gc_values(|_,_,_,_|Ok(()),|_,_|Err(invalid("synthetic-unit-observer-failure"))).is_err());
        let barrier=store.acquire_plugin_gc_barrier(&collect(&store).0).unwrap(); drop(barrier);
        store.connection().execute("UPDATE lww_units SET value=?1 WHERE key=?2",params![r#"{"kind":"inline","bytes":"not canonical base64!"}"#,key.as_str()]).unwrap();
        assert!(store.visit_plugin_gc_values(|_,_,_,_|Ok(()),|_,_|Ok(())).is_err());
        store.connection().execute("UPDATE lww_units SET value=?1,key=?2 WHERE key=?3",params![serde_json::to_string(&UnitValue::Deleted).unwrap(),r#"["plugin-local","invalid-shape"]"#,key.as_str()]).unwrap();
        assert!(store.visit_plugin_gc_values(|_,_,_,_|Ok(()),|_,_|Ok(())).is_err());
    }
}
