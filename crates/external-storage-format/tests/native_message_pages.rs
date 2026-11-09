use std::fmt;

// Errors keep only their text so results compare, as the stored JSON tests compiled below require.
#[derive(Debug, PartialEq, Eq)]
pub enum StoreError {
    Sql(String),
    Json(String),
    Validation { message: String },
}
impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sql(message) | Self::Json(message) | Self::Validation { message } => f.write_str(message),
        }
    }
}
impl std::error::Error for StoreError {}
impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sql(e.to_string())
    }
}
impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e.to_string())
    }
}
pub type StoreResult<T> = Result<T, StoreError>;
mod record_projection {
    pub fn codec_error(error: impl std::fmt::Display) -> super::StoreError {
        super::StoreError::Validation {
            message: error.to_string(),
        }
    }
}
#[path = "../../../src-tauri/src/persistent_store/hash_work.rs"]
pub(crate) mod native_hash_work;
mod persistent_store {
    pub(crate) use crate::native_hash_work as hash_work;
}
#[allow(dead_code)]
#[path = "../../../src-tauri/src/persistent_store/stored_json.rs"]
mod stored_json;
mod lww {
    pub(crate) fn error(message: impl ToString) -> super::StoreError {
        super::StoreError::Validation {
            message: message.to_string(),
        }
    }
    // Copied-database acceptance needs the full native LWW store. This page
    // fixture must reject any attempt to cross that unavailable boundary.
    pub(crate) fn validate_received(
        _db: &rusqlite::Connection,
        _key: &risunest_sync_wire::unit::UnitKey,
        _value: &risunest_sync_wire::unit::UnitValue,
    ) -> super::StoreResult<()> {
        Err(super::record_projection::codec_error(
            "native LWW validation is unavailable in the isolated page fixture",
        ))
    }
    #[allow(dead_code)]
    pub(crate) fn archive_object_hashes(
        _archived: &super::archive::ArchivedObject,
    ) -> super::StoreResult<Vec<String>> {
        Err(super::record_projection::codec_error(
            "native archive roots are unavailable in the isolated page fixture",
        ))
    }
}
mod archive {
    #[allow(dead_code)]
    pub(crate) type ArchivedObject = serde_json::Value;
}
mod device_store {
    pub(crate) mod plugin_values {
        pub(crate) mod structure {
            use crate::{record_projection::codec_error, StoreResult};
            #[allow(dead_code)]
            pub(crate) fn row_parts(
                _db: &rusqlite::Connection,
                _schema: &str,
                _visit: impl FnMut(&str) -> StoreResult<()>,
            ) -> StoreResult<()> {
                Err(codec_error(
                    "native plugin value roots are unavailable in the isolated page fixture",
                ))
            }
        }
    }
}
// The sweep and copied-database acceptance are outside this fixture.
#[allow(dead_code)]
#[path = "../../../src-tauri/src/persistent_store/message_pages.rs"]
mod message_pages;

#[cfg(test)]
mod tests {
    use super::*;
    use message_pages::*;
    use risunest_external_storage_format::message_pages::{MessageHash, MessageManifest};
    use risunest_sync_wire::{payload_value, unit::UnitValue};
    use rusqlite::{params, Connection};
    use serde_json::{json, Value};

    fn fixture() -> Connection {
        initialize(Connection::open_in_memory().unwrap())
    }
    fn initialize(db: Connection) -> Connection {
        db.execute_batch("CREATE TABLE messages(generation TEXT,character_id TEXT,conversation_id TEXT,message_index INTEGER,
            message_id TEXT,value TEXT,canonical_hash TEXT NOT NULL DEFAULT '',canonical_size INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY(generation,character_id,conversation_id,message_index));
            CREATE TABLE conversations(generation TEXT,character_id TEXT,conversation_id TEXT,message_count INTEGER,
            PRIMARY KEY(generation,character_id,conversation_id));
            INSERT INTO conversations VALUES('g','c','chat',0);").unwrap();
        db.execute_batch(SCHEMA).unwrap();
        db
    }

    fn copy_objects(source: &Connection, target: &Connection) {
        let mut statement = source.prepare("SELECT hash,body FROM message_page_objects").unwrap();
        let rows = statement.query_map([], |r| Ok((r.get::<_,String>(0)?,r.get::<_,Vec<u8>>(1)?))).unwrap();
        for row in rows {
            let (hash,body) = row.unwrap();
            put_object(target, &hash, &body).unwrap();
        }
    }

    fn splice(db: &Connection, start: usize, delete: usize, values: &[Value]) {
        db.execute("DELETE FROM messages WHERE message_index>=?1 AND message_index<?2",
            params![start as i64,(start+delete) as i64]).unwrap();
        let delta = values.len() as i64-delete as i64;
        if delta != 0 {
            db.execute("UPDATE messages SET message_index=-(message_index+?1)-1 WHERE message_index>=?2",
                params![delta,(start+delete) as i64]).unwrap();
            db.execute("UPDATE messages SET message_index=-message_index-1 WHERE message_index<0", []).unwrap();
        }
        insert(db,start,values);
    }

    fn unit_for_manifest(db: &Connection, manifest: &MessageManifest) -> UnitValue {
        let object = manifest.encode().unwrap();
        put_object(db,&object.hash,&object.bytes).unwrap();
        let mut descriptor = risunest_sync_wire::descriptor::RecordDescriptor::content(object.hash);
        descriptor.dependencies = manifest.pages.iter().map(|p| p.hash.clone()).collect();
        descriptor.dependencies.sort(); descriptor.dependencies.dedup();
        UnitValue::object(descriptor).unwrap()
    }
    fn insert(db: &Connection, start: usize, values: &[Value]) {
        for (offset, value) in values.iter().enumerate() {
            let body = payload_value::encode(value).unwrap();
            let hash = MessageHash::from_bytes(&body);
            db.execute(
                "INSERT INTO messages VALUES('g','c','chat',?1,?2,?3,?4,?5)",
                params![
                    (start + offset) as i64,
                    value.get("chatId").and_then(Value::as_str),
                    String::from_utf8(body).unwrap(),
                    hash.hash,
                    hash.byte_length as i64
                ],
            )
            .unwrap();
        }
        db.execute(
            "UPDATE conversations SET message_count=(SELECT count(*) FROM messages)",
            [],
        )
        .unwrap();
    }
    fn synthetic() -> Vec<Value> {
        (0..1024)
            .map(|i| json!({"chatId":format!("id-{i}"),"data":format!("synthetic-{i}")}))
            .collect()
    }

    #[test]
    fn incremental_staging_matches_fresh_native_and_format_pages() {
        use risunest_external_storage_format::message_pages::{repage, PageIndex, MAX_PAGE_BYTES};
        let mut messages = synthetic();
        messages.extend((0..13).map(|index| json!({"chatId":format!("tail-{index}"),"data":"partial tail"})));
        messages[300]["data"] = json!("x".repeat(MAX_PAGE_BYTES + 1));
        let bodies = messages.iter().map(|value| payload_value::encode(value).unwrap()).collect::<Vec<_>>();
        let format = repage::<StoreError>(&PageIndex { messages: vec![], pages: vec![] },
            bodies.iter().map(|body| MessageHash::from_bytes(body)).collect(), 0..0, bodies.len(),
            |index| Ok(bodies[index].clone())).unwrap();
        for batch in [7, 129, 513] {
            let mut db = fixture();
            for (index, values) in messages.chunks(batch).enumerate() {
                let tx = db.transaction().unwrap();
                insert(&tx, index * batch, values);
                tx.execute("UPDATE conversations SET message_count=?1", [messages.len() as i64]).unwrap();
                reset_capture_work();
                stage_conversation_pages(&tx, "g", "c", "chat", &|| Ok(())).unwrap();
                let work = take_capture_work().work;
                assert!(work.messages_read <= values.len() + 128, "batch {batch}: {work:?}");
                assert!(work.peak_page_messages <= 128, "{work:?}");
                tx.commit().unwrap();
            }
            assert_eq!(load_pages(&db, "g", "c", "chat").unwrap(), format.index.pages);
            let tx = db.transaction().unwrap();
            let staged = current_manifest(&tx, "g", "c", "chat").unwrap();
            assert_eq!(manifest(&tx, &staged), format.index.manifest());
            assert_eq!(staged, capture_manifest(&tx, "g", "c", "chat", None).unwrap());
            tx.rollback().unwrap();
        }
    }
    fn manifest(db: &Connection, value: &UnitValue) -> MessageManifest {
        let UnitValue::Object { descriptor, .. } = value else {
            panic!("object expected")
        };
        MessageManifest::decode(&object_body(db, &descriptor.object_hash).unwrap().unwrap())
            .unwrap()
    }

    #[test]
    fn persisted_restart_range_updates_match_full_capture_and_preserve_message_ids() {
        for (name, start, delete, values) in [
            (
                "append",
                1024,
                0,
                vec![json!({"chatId":"appended","data":"new"})],
            ),
            (
                "edit",
                500,
                1,
                vec![json!({"chatId":"id-500","data":"edited"})],
            ),
            (
                "insert",
                500,
                0,
                vec![json!({"chatId":"inserted","data":"new"})],
            ),
            ("delete", 500, 1, vec![]),
        ] {
            let mut db = fixture();
            insert(&db, 0, &synthetic());
            let tx = db.transaction().unwrap();
            capture_manifest(&tx, "g", "c", "chat", None).unwrap();
            tx.commit().unwrap();
            // Only database state survives this boundary, no retained in-memory pager.
            let tx = db.transaction().unwrap();
            let before = current_manifest(&tx, "g", "c", "chat").unwrap();
            tx.execute(
                "DELETE FROM messages WHERE message_index>=?1 AND message_index<?2",
                params![start as i64, (start + delete) as i64],
            )
            .unwrap();
            let delta = values.len() as i64 - delete as i64;
            if delta != 0 {
                tx.execute("UPDATE messages SET message_index=-(message_index+?1)-1 WHERE message_index>=?2",params![delta,(start+delete) as i64]).unwrap();
                tx.execute(
                    "UPDATE messages SET message_index=-message_index-1 WHERE message_index<0",
                    [],
                )
                .unwrap();
            }
            insert(&tx, start, &values);
            let (updated, work) = capture_with_work(
                &tx,
                "g",
                "c",
                "chat",
                Some(&[MessageEdit {
                    start: start as i64,
                    delete_count: delete as i64,
                    insert_count: values.len() as i64,
                }]),
            )
            .unwrap();
            let updated_manifest = manifest(&tx, &updated);
            let full = capture_manifest(&tx, "g", "c", "chat", None).unwrap();
            assert_eq!(updated, full, "{name}");
            assert_ne!(before, updated, "{name}");
            assert!(work.messages_read < 256, "{name}: {work:?}");
            assert!(
                work.hash_rows_read <= work.messages_read + 1,
                "{name}: {work:?}"
            );
            let mut recipient = fixture();
            for hash in updated_manifest
                .pages
                .iter()
                .map(|p| p.hash.as_str())
                .chain(match &updated {
                    UnitValue::Object { descriptor, .. } => vec![descriptor.object_hash.as_str()],
                    _ => unreachable!(),
                })
            {
                put_object(&recipient, hash, &object_body(&tx, hash).unwrap().unwrap()).unwrap();
            }
            let remote = recipient.transaction().unwrap();
            apply_manifest(&remote, "g", "c", "chat", &updated).unwrap();
            validate_manifest(&remote, &updated).unwrap();
            let ids = remote
                .prepare("SELECT message_id FROM messages ORDER BY message_index")
                .unwrap()
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(ids.len(), updated_manifest.message_count.0 as usize);
            if name == "edit" {
                assert_eq!(ids[500], "id-500");
            }
            println!("native-{name}: {work:?}");
        }
    }

    #[test]
    fn receive_append_edit_insert_delete_reuses_hashes_and_native_rows_after_reopen() {
        use native_hash_work::{reset_hash_work,take_hash_work,DomainWork};
        for (name,start,delete,values) in [
            ("append",1024,0,vec![json!({"chatId":"appended","data":"new"})]),
            ("edit",500,1,vec![json!({"chatId":"id-500","data":"edited"})]),
            ("insert",500,0,vec![json!({"chatId":"inserted","data":"new"})]),
            ("delete",500,1,vec![]),
        ] {
            let mut source = fixture(); insert(&source,0,&synthetic());
            let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let directory = std::env::temp_dir().join(format!("risunest-page-receive-{}-{nonce}-{name}",std::process::id()));
            std::fs::create_dir(&directory).unwrap(); let path = directory.join("pages.sqlite");
            let mut target = initialize(Connection::open(&path).unwrap()); insert(&target,0,&synthetic());
            let tx = target.transaction().unwrap();
            let before = capture_manifest(&tx,"g","c","chat",None).unwrap(); tx.commit().unwrap();
            let old_manifest = manifest(&target,&before);
            let tx = source.transaction().unwrap(); capture_manifest(&tx,"g","c","chat",None).unwrap();
            splice(&tx,start,delete,&values);
            let updated = capture_manifest(&tx,"g","c","chat",Some(&[MessageEdit {
                start:start as i64,delete_count:delete as i64,insert_count:values.len() as i64,
            }])).unwrap();
            let new_manifest = manifest(&tx,&updated);
            let new_pages = new_manifest.pages.iter().filter(|p| !old_manifest.pages.iter().any(|old| old.hash==p.hash)).collect::<Vec<_>>();
            let hashed_messages = new_pages.iter().map(|p| p.message_count as u64).sum::<u64>();
            let hashed_bytes = new_pages.iter().map(|p| p.byte_length.0-(risunest_external_storage_format::message_pages::PAGE_PREFIX.len()
                +risunest_external_storage_format::message_pages::PAGE_SUFFIX.len()) as u64-(p.message_count as u64-1)).sum::<u64>();
            copy_objects(&tx,&target);
            target.execute_batch("CREATE TABLE row_writes(kind TEXT,message_index INTEGER);
                CREATE TRIGGER audit_insert AFTER INSERT ON messages BEGIN INSERT INTO row_writes VALUES('insert',NEW.message_index); END;
                CREATE TRIGGER audit_delete AFTER DELETE ON messages BEGIN INSERT INTO row_writes VALUES('delete',OLD.message_index); END;").unwrap();
            let old_rowids = target.prepare("SELECT message_id,rowid FROM messages ORDER BY message_index").unwrap()
                .query_map([],|r| Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?))).unwrap()
                .collect::<Result<std::collections::BTreeMap<_,_>,_>>().unwrap();
            reset_hash_work();
            let admission = target.transaction().unwrap(); validate_manifest(&admission,&updated).unwrap(); admission.commit().unwrap();
            let work = take_hash_work();
            assert_eq!(work.domains.get("native_message_verify").cloned().unwrap_or_default(),
                DomainWork { calls:hashed_messages,bytes:hashed_bytes },"{name}");
            assert_eq!(work.domains.get("native_page_decode_identity").cloned().unwrap_or_default().calls,new_pages.len() as u64,"{name}");
            assert!(!work.domains.contains_key("native_repage_message_verify"));
            assert!(!work.domains.contains_key("native_repage_page_identity"));
            assert!(work.incomplete.is_empty());
            assert!(hashed_messages<256,"fixed {name} fixture must reuse unchanged pages");
            drop(target); let mut target = Connection::open(&path).unwrap();
            reset_hash_work();
            let apply = target.transaction().unwrap(); apply_manifest(&apply,"g","c","chat",&updated).unwrap(); apply.commit().unwrap();
            let work = take_hash_work();
            for domain in ["native_message_verify","native_page_decode_identity","native_repage_message_verify","native_repage_page_identity"] {
                assert!(!work.domains.contains_key(domain),"{name}: persisted admission proofs must skip {domain}");
            }
            assert!(work.incomplete.is_empty());
            let count = |kind| target.query_row("SELECT count(*) FROM row_writes WHERE kind=?1",[kind],|r| r.get::<_,i64>(0)).unwrap();
            assert_eq!(count("insert"),values.len() as i64,"{name}"); assert_eq!(count("delete"),delete as i64,"{name}");
            let actual = target.prepare("SELECT message_id,value,rowid FROM messages ORDER BY message_index").unwrap()
                .query_map([],|r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?))).unwrap()
                .collect::<Result<Vec<_>,_>>().unwrap();
            let mut expected = synthetic(); expected.splice(start..start+delete,values.clone());
            assert_eq!(actual.iter().map(|(_,body,_)| serde_json::from_str::<Value>(body).unwrap()).collect::<Vec<_>>(),expected,"{name}");
            for (id,_,rowid) in &actual {
                if id=="id-500" && delete!=0 { continue; }
                if let Some(old) = old_rowids.get(id) { assert_eq!(rowid,old,"{name}: unchanged {id}"); }
            }
            let tx = target.transaction().unwrap();
            assert_eq!(current_manifest(&tx,"g","c","chat").unwrap(),updated);
            assert_eq!(capture_manifest(&tx,"g","c","chat",None).unwrap(),updated,"{name}");
            tx.commit().unwrap(); drop(target);
            std::fs::remove_file(&path).unwrap(); std::fs::remove_dir(&directory).unwrap();
        }
    }

    #[test]
    fn cached_pages_still_require_cross_boundary_lookahead_and_live_immutable_objects() {
        use risunest_external_storage_format::{logical_records::encode_message_page,
            message_pages::{ManifestPage,MANIFEST_SCHEMA}};
        use native_hash_work::{reset_hash_work,take_hash_work,DomainWork};
        let mut source = fixture(); let mut target = fixture();
        let values = vec![json!({"chatId":"first","data":"small"}),
            json!({"chatId":"huge","data":"x".repeat(256*1024)}),
            json!({"chatId":"last","data":"small"})];
        insert(&source,0,&values); let tx = source.transaction().unwrap();
        let before = capture_manifest(&tx,"g","c","chat",None).unwrap();
        let old = manifest(&tx,&before); assert_eq!(old.pages.len(),3);
        copy_objects(&tx,&target); let remote = target.transaction().unwrap();
        apply_manifest(&remote,"g","c","chat",&before).unwrap(); remote.commit().unwrap();
        let first_body = require_body(&tx,&old.pages[0].hash);
        // Certify another final remainder in isolation. Its certificate cannot
        // authorize joining it to a formerly size-cut preceding page.
        let edited = json!({"chatId":"huge","data":"edited"});
        let tail = encode_message_page(&[edited.clone(),values[2].clone()]).unwrap();
        put_object(&target,&tail.hash,&tail.bytes).unwrap();
        let tail_page = ManifestPage { hash:tail.hash,message_count:2,byte_length:tail.size.into() };
        let isolated = MessageManifest { schema:MANIFEST_SCHEMA.into(),message_count:2.into(),pages:vec![tail_page.clone()] };
        let isolated = unit_for_manifest(&target,&isolated); validate_manifest(&target,&isolated).unwrap();
        let forged = MessageManifest { schema:MANIFEST_SCHEMA.into(),message_count:3.into(),pages:vec![old.pages[0].clone(),tail_page] };
        let forged = unit_for_manifest(&target,&forged);
        assert!(validate_manifest(&target,&forged).is_err());
        let remote = target.transaction().unwrap();
        assert!(apply_manifest(&remote,"g","c","chat",&forged).is_err()); remote.rollback().unwrap();
        assert_eq!(target.query_row("SELECT value FROM messages WHERE message_index=1",[],|r| r.get::<_,String>(0)).unwrap(),
            String::from_utf8(payload_value::encode(&values[1]).unwrap()).unwrap());
        splice(&tx,1,1,&[edited]);
        let updated = capture_manifest(&tx,"g","c","chat",Some(&[MessageEdit { start:1,delete_count:1,insert_count:1 }])).unwrap();
        let after = manifest(&tx,&updated); assert_eq!(after.pages.len(),1);
        copy_objects(&tx,&target);
        reset_hash_work(); validate_manifest(&target,&updated).unwrap();
        let work = take_hash_work();
        assert_eq!(work.domains["native_message_verify"].calls,3);
        assert_eq!(work.domains["native_page_decode_identity"],DomainWork { calls:1,bytes:after.pages[0].byte_length.0 });
        let remote = target.transaction().unwrap(); apply_manifest(&remote,"g","c","chat",&updated).unwrap(); remote.commit().unwrap();
        let hash = &old.pages[0].hash;
        assert!(verified_object_present(&target,hash).unwrap());
        assert!(target.execute("UPDATE message_page_objects SET body=?2 WHERE hash=?1",params![hash,b"wrong".as_slice()]).is_err());
        assert!(target.execute("INSERT OR REPLACE INTO message_page_objects VALUES(?1,?2)",params![hash,b"wrong".as_slice()]).is_err());
        assert!(target.execute("UPDATE message_page_proofs SET ends_cut=1 WHERE hash=?1",[hash]).is_err());
        target.execute("DELETE FROM message_page_objects WHERE hash=?1",[hash]).unwrap();
        assert!(!verified_object_present(&target,hash).unwrap());
        assert_eq!(target.query_row("SELECT count(*) FROM message_page_proofs WHERE hash=?1",[hash],|r| r.get::<_,i64>(0)).unwrap(),0);
        assert!(validate_manifest(&target,&before).is_err());
        let remote = target.transaction().unwrap(); assert!(apply_manifest(&remote,"g","c","chat",&before).is_err()); remote.rollback().unwrap();
        target.execute("INSERT INTO message_page_objects VALUES(?1,?2)",params![hash,b"wrong".as_slice()]).unwrap();
        assert!(!verified_object_present(&target,hash).unwrap());
        assert!(validate_manifest(&target,&before).is_err());
        assert!(put_object(&target,hash,&first_body).is_err());
        assert!(!verified_object_present(&target,hash).unwrap());
    }

    #[test]
    fn cached_page_count_length_dependencies_and_internal_cuts_remain_checked() {
        use risunest_external_storage_format::{logical_records::encode_message_page,
            message_pages::{ManifestPage,MANIFEST_SCHEMA}};
        let mut db = fixture(); let values = synthetic(); insert(&db,0,&values);
        let tx = db.transaction().unwrap(); let value = capture_manifest(&tx,"g","c","chat",None).unwrap();
        let original = manifest(&tx,&value);
        let rejects = |forged: &MessageManifest| {
            let value = unit_for_manifest(&tx,forged);
            assert!(validate_manifest(&tx,&value).is_err());
            assert!(apply_manifest(&tx,"g","c","chat",&value).is_err());
            assert_eq!(tx.query_row("SELECT count(*) FROM messages",[],|r|r.get::<_,i64>(0)).unwrap(),1024);
        };
        let mut wrong_count = original.clone(); wrong_count.pages[0].message_count-=1; wrong_count.message_count.0-=1;
        rejects(&wrong_count);
        let mut wrong_length = original.clone(); wrong_length.pages[0].byte_length.0+=1;
        rejects(&wrong_length);
        let UnitValue::Object { mut descriptor,.. } = value else { unreachable!() };
        descriptor.dependencies.clear();
        let wrong_dependencies = UnitValue::object(descriptor).unwrap();
        assert!(validate_manifest(&tx,&wrong_dependencies).is_err());
        assert!(apply_manifest(&tx,"g","c","chat",&wrong_dependencies).is_err());
        let combined_count = original.pages[0].message_count+original.pages[1].message_count;
        assert!(combined_count<=128);
        let page = encode_message_page(&values[..combined_count as usize]).unwrap();
        put_object(&tx,&page.hash,&page.bytes).unwrap();
        let forged = MessageManifest { schema:MANIFEST_SCHEMA.into(),message_count:(combined_count as u64).into(),
            pages:vec![ManifestPage { hash:page.hash,message_count:combined_count,byte_length:page.size.into() }] };
        // A final remainder still cannot contain a content-defined earlier cut.
        rejects(&forged);
    }

    #[test]
    fn remote_missing_body_or_tampered_manifest_changes_nothing() {
        let mut source = fixture();
        insert(&source, 0, &synthetic()[..24]);
        let tx = source.transaction().unwrap();
        let value = capture_manifest(&tx, "g", "c", "chat", None).unwrap();
        let mut target = fixture();
        insert(&target, 0, &[json!({"chatId":"kept","data":"local"})]);
        let remote = target.transaction().unwrap();
        assert!(apply_manifest(&remote, "g", "c", "chat", &value).is_err());
        assert!(validate_manifest(&remote, &value).is_err());
        let UnitValue::Object { descriptor, .. } = &value else {
            unreachable!()
        };
        put_object(
            &remote,
            &descriptor.object_hash,
            &require_body(&tx, &descriptor.object_hash),
        )
        .unwrap();
        assert!(apply_manifest(&remote, "g", "c", "chat", &value).is_err());
        assert_eq!(
            remote
                .query_row("SELECT message_id FROM messages", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            "kept"
        );
        assert!(put_object(&remote, &"00".repeat(32), b"wrong").is_err());
        let values = synthetic();
        let mut forged = manifest(&tx, &value);
        forged.pages.clear();
        for chunk in values[..24].chunks(12) {
            let page =
                risunest_external_storage_format::logical_records::encode_message_page(chunk)
                    .unwrap();
            put_object(&remote, &page.hash, &page.bytes).unwrap();
            forged.pages.push(
                risunest_external_storage_format::message_pages::ManifestPage {
                    hash: page.hash,
                    message_count: chunk.len() as u32,
                    byte_length: risunest_sync_wire::stamp::DecimalU64(page.size),
                },
            );
        }
        let object = forged.encode().unwrap();
        put_object(&remote, &object.hash, &object.bytes).unwrap();
        let mut descriptor = risunest_sync_wire::descriptor::RecordDescriptor::content(object.hash);
        descriptor.dependencies = forged.pages.iter().map(|p| p.hash.clone()).collect();
        descriptor.dependencies.sort();
        descriptor.dependencies.dedup();
        let forged = UnitValue::object(descriptor).unwrap();
        assert!(validate_manifest(&remote, &forged).is_err());
        assert!(apply_manifest(&remote, "g", "c", "chat", &forged).is_err());
        assert_eq!(
            remote
                .query_row("SELECT message_id FROM messages", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            "kept"
        );
    }
    fn require_body(db: &Connection, hash: &str) -> Vec<u8> {
        object_body(db, hash).unwrap().unwrap()
    }

    #[test]
    fn empty_manifest_and_upstream_uncached_messages_are_supported() {
        let mut db = fixture();
        let tx = db.transaction().unwrap();
        let empty = capture_manifest(&tx, "g", "c", "chat", None).unwrap();
        assert_eq!(manifest(&tx, &empty).message_count.0, 0);
        tx.execute("INSERT INTO messages(generation,character_id,conversation_id,message_index,message_id,value)
            VALUES('g','c','chat',0,'upstream','{\"chatId\":\"upstream\",\"data\":0.1}')",[]).unwrap();
        tx.execute("UPDATE conversations SET message_count=1", [])
            .unwrap();
        capture_manifest(&tx, "g", "c", "chat", None).unwrap();
        let hash: String = tx
            .query_row("SELECT canonical_hash FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(hash.len(), 64);
    }

    #[test]
    fn edits_on_size_cut_restart_the_preceding_page_and_rollback_is_atomic() {
        let mut db = fixture();
        let values = vec![
            json!({"chatId":"first","data":"small"}),
            json!({"chatId":"huge","data":"x".repeat(256*1024)}),
            json!({"chatId":"last","data":"small"}),
        ];
        insert(&db, 0, &values);
        let tx = db.transaction().unwrap();
        let old = capture_manifest(&tx, "g", "c", "chat", None).unwrap();
        tx.commit().unwrap();
        {
            let tx = db.transaction().unwrap();
            tx.execute("DELETE FROM messages WHERE message_index=1", [])
                .unwrap();
            insert(&tx, 1, &[json!({"chatId":"huge","data":"edited"})]);
            let (updated, work) = capture_with_work(
                &tx,
                "g",
                "c",
                "chat",
                Some(&[MessageEdit {
                    start: 1,
                    delete_count: 1,
                    insert_count: 1,
                }]),
            )
            .unwrap();
            assert_ne!(old, updated);
            assert_eq!(work.prefix_reused, 0);
            assert_eq!(manifest(&tx, &updated).pages.len(), 1);
            assert_eq!(
                capture_manifest(&tx, "g", "c", "chat", None).unwrap(),
                updated
            );
            tx.rollback().unwrap();
        }
        let tx = db.transaction().unwrap();
        assert_eq!(current_manifest(&tx, "g", "c", "chat").unwrap(), old);
        assert_eq!(capture_manifest(&tx, "g", "c", "chat", None).unwrap(), old);
    }
}
