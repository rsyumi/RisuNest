//! Only validated records enter this private, disk-backed preparation.
//! SQLite bounds its page cache; activation materializes one record at a time.
use super::*;

pub(crate) struct ValidatedRecords {
    db: rusqlite::Connection,
    _file: tempfile::NamedTempFile,
    len: usize,
}
impl ValidatedRecords {
    pub fn new() -> StoreResult<Self> {
        let file = tempfile::NamedTempFile::new()?;
        let db = rusqlite::Connection::open(file.path())?;
        db.execute_batch(
            "PRAGMA journal_mode=OFF; PRAGMA cache_size=-1024; PRAGMA mmap_size=0;
            CREATE TABLE records(key TEXT PRIMARY KEY,priority INTEGER NOT NULL,body TEXT NOT NULL,digest TEXT NOT NULL);
            CREATE INDEX records_order ON records(priority,key);",
        )?;
        Ok(Self {
            db,
            _file: file,
            len: 0,
        })
    }
    pub fn push(&mut self, record: ValidatedRecord) -> StoreResult<()> {
        let priority = match (&record.record.payload, &record.locator) {
            (None, _) => 0,
            (Some(_), LogicalRecordLocator::Root) => 1,
            (Some(_), LogicalRecordLocator::Character { .. }) => 2,
            _ => 3,
        };
        let body = serde_json::to_string(&record.record)?;
        self.db.execute(
            "INSERT INTO records VALUES(?1,?2,?3,?4)",
            params![
                record.record.key,
                priority,
                body,
                risunest_sync_wire::hash(body.as_bytes())
            ],
        )?;
        self.len += 1;
        Ok(())
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn clear(&mut self) -> StoreResult<()> {
        self.db.execute("DELETE FROM records", [])?;
        self.len = 0;
        Ok(())
    }
    pub fn deletes(&self, key: &str) -> StoreResult<bool> {
        Ok(self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM records WHERE key=?1 AND priority=0)",
            [key],
            |r| r.get(0),
        )?)
    }
    pub fn visit(
        &self,
        only_deletions: bool,
        mut operation: impl FnMut(ValidatedRecord) -> StoreResult<()>,
    ) -> StoreResult<()> {
        let mut statement = self.db.prepare(if only_deletions {
            "SELECT key,body,digest FROM records WHERE priority=0 ORDER BY key"
        } else {
            "SELECT key,body,digest FROM records ORDER BY priority,key"
        })?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let key: String = row.get(0)?;
            let body: String = row.get(1)?;
            if risunest_sync_wire::hash(body.as_bytes()) != row.get::<_, String>(2)? {
                return invalid("Server preparation content changed");
            }
            let record: RemoteRecord = serde_json::from_str(&body)?;
            if record.key != key {
                return invalid("Server preparation key changed");
            }
            let locator =
                decode_logical_record_key(&record.key).map_err(|_| StoreError::Validation {
                    message: "Invalid server preparation key".into(),
                })?;
            operation(ValidatedRecord { record, locator })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn server_sync_staging_pages_payloads_and_rejects_corruption_before_delivery() {
        let root = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(root.path()).unwrap();
        let mut staged = ValidatedRecords::new().unwrap();
        for i in 0..256u64 {
            let locator = LogicalRecordLocator::Plugin {
                owner: "synthetic-plugin".to_owned(),
                storage_key: format!("synthetic-{i:04}"),
            };
            let record = RemoteRecord {
                key: encode_logical_record_key(&locator).unwrap(),
                version: RecordVersion::Live {
                    object_hash: "a".repeat(64),
                    descriptor_hash: None,
                },
                payload: Some(ServerPayload {
                    record: LogicalRecordEnvelope::Plugin {
                        owner: "synthetic-plugin".to_owned(),
                        ordinal: i,
                        value: Value::String("x".repeat(16 * 1024)),
                    },
                    messages: None,
                    derived_objects: Default::default(),
                }),
                local_hash: None,
            };
            staged.push(validate_remote(record, &cas).unwrap()).unwrap();
        }
        assert_eq!(staged.len(), 256);
        assert!(staged._file.as_file().metadata().unwrap().len() > 4 * 1024 * 1024);
        let mut delivered_ordinals = BTreeSet::new();
        staged
            .visit(false, |item| {
                let Some(ServerPayload {
                    record: LogicalRecordEnvelope::Plugin { ordinal, value, .. },
                    ..
                }) = item.record.payload
                else {
                    panic!("wrong staged family")
                };
                let LogicalRecordLocator::Plugin { storage_key, .. } = item.locator else {
                    panic!("wrong locator")
                };
                assert_eq!(storage_key, format!("synthetic-{ordinal:04}"));
                assert_eq!(value.as_str().unwrap().len(), 16 * 1024);
                assert!(delivered_ordinals.insert(ordinal));
                Ok(())
            })
            .unwrap();
        assert_eq!(delivered_ordinals, (0..256).collect());
        staged
            .db
            .execute(
                "UPDATE records SET body=replace(body,'xxxxxxxx','yyyyyyyy')",
                [],
            )
            .unwrap();
        let mut delivered = 0;
        assert!(staged
            .visit(false, |_| {
                delivered += 1;
                Ok(())
            })
            .is_err());
        assert_eq!(delivered, 0);
        let path = staged._file.path().to_owned();
        drop(staged);
        assert!(!path.exists());
    }
}
