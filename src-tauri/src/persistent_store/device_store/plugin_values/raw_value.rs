//! Binary plugin values stay bytes in SQLite, section objects and native IPC.
use super::super::{invalid, DeviceStore};
use crate::persistent_store::StoreResult;
use rusqlite::{params, Connection, MAIN_DB, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{self, Read, Seek, SeekFrom};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BinaryInfo { pub(crate) byte_length: u64, pub(crate) hash: String }

pub(crate) fn is_object(db: &Connection, hash: &str) -> StoreResult<bool> {
    let (rowid, _) = super::text_value::object_row(db, hash)?;
    let mut input = db.blob_open(MAIN_DB, "message_page_objects", "body", rowid, true)?;
    let mut first = [0];
    Ok(input.read(&mut first)? == 1 && first[0] == 0)
}

pub(crate) fn inspect(mut input: impl Read) -> StoreResult<BinaryInfo> {
    let mut hash = Sha256::new();
    let mut header = [0; 13];
    input.read_exact(&mut header)?;
    if header[..5] != [0, 80, 76, 83, 1] { return Err(invalid("unsupported plugin binary encoding")); }
    let metadata = u32::from_le_bytes(header[5..9].try_into().unwrap()) as u64;
    let root = u32::from_le_bytes(header[9..13].try_into().unwrap()) as u64;
    hash.update(header);
    let mut length = 13;
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 { break; }
        length += count as u64;
        if length > super::text_value::MAX_VALUE_BYTES { return Err(invalid("plugin device value is too large")); }
        hash.update(&buffer[..count]);
    }
    if metadata == 0 || root == 0 || 13 + metadata + root > length { return Err(invalid("plugin binary value is truncated")); }
    Ok(BinaryInfo { byte_length: length, hash: hex::encode(hash.finalize()) })
}

pub(crate) fn inspect_object(db: &Connection, hash: &str) -> StoreResult<BinaryInfo> {
    let (rowid, _) = super::text_value::object_row(db, hash)?;
    let info = inspect(db.blob_open(MAIN_DB, "message_page_objects", "body", rowid, true)?)?;
    if info.hash != hash { return Err(invalid("plugin binary object hash mismatch")); }
    Ok(info)
}

pub(crate) fn store<R: Read>(db: &Connection, mut source: impl FnMut() -> StoreResult<R>) -> StoreResult<BinaryInfo> {
    let info = inspect(source()?)?;
    let exists: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM message_page_objects WHERE hash=?1)", [&info.hash], |row| row.get(0))?;
    if !exists {
        db.execute("INSERT INTO message_page_objects(hash,body) VALUES(?1,zeroblob(?2))", params![info.hash, info.byte_length as i64])?;
        let mut output = db.blob_open(MAIN_DB, "message_page_objects", "body", db.last_insert_rowid(), false)?;
        if io::copy(&mut source()?, &mut output)? != info.byte_length { return Err(invalid("plugin binary object is incomplete")); }
        output.close()?;
    }
    if inspect_object(db, &info.hash)? != info { return Err(invalid("plugin binary object changed")); }
    db.execute("INSERT INTO message_page_verified_objects(hash) SELECT ?1 WHERE NOT EXISTS(SELECT 1 FROM message_page_verified_objects WHERE hash=?1)", [&info.hash])?;
    db.execute("DELETE FROM message_page_object_marks WHERE hash=?1", [&info.hash])?;
    Ok(info)
}

pub(crate) fn copy_object(source: &Connection, target: &Connection, info: &BinaryInfo) -> StoreResult<()> {
    let (rowid, _) = super::text_value::object_row(source, &info.hash)?;
    if store(target, || Ok(source.blob_open(MAIN_DB, "message_page_objects", "body", rowid, true)?))? != *info {
        return Err(invalid("plugin binary object identity differs"));
    }
    Ok(())
}

pub(crate) fn write_reserved(db: &Connection, owner: &str, key: &str, info: &BinaryInfo, clock: &str, writer: &str,
    fill: impl FnOnce(&mut rusqlite::blob::Blob<'_>) -> StoreResult<()>) -> StoreResult<()> {
    if info.byte_length == 0 || info.byte_length > super::text_value::MAX_VALUE_BYTES { return Err(invalid("plugin device value is too large")); }
    db.execute("INSERT INTO plugin_device_storage
        (owner,space,key,value,byte_size,tombstone,write_clock,writer_id,published_clock,first_published_generation,first_published_at_ms)
        VALUES(?1,'json',?2,zeroblob(?3),?3,0,?4,?5,NULL,NULL,NULL)
        ON CONFLICT(owner,space,key) DO UPDATE SET value=excluded.value,byte_size=excluded.byte_size,tombstone=0,
        write_clock=excluded.write_clock,writer_id=excluded.writer_id,published_clock=NULL,
        first_published_generation=NULL,first_published_at_ms=NULL", params![owner,key,info.byte_length as i64,clock,writer])?;
    let rowid = db.query_row("SELECT rowid FROM plugin_device_storage WHERE owner=?1 AND space='json' AND key=?2", params![owner,key], |row| row.get(0))?;
    let mut output = db.blob_open(MAIN_DB, "plugin_device_storage", "value", rowid, false)?;
    fill(&mut output)?;
    output.close()?;
    if inspect(db.blob_open(MAIN_DB, "plugin_device_storage", "value", rowid, true)?)? != *info { return Err(invalid("plugin binary write identity differs")); }
    Ok(())
}

impl DeviceStore {
    pub(crate) fn plugin_value_is_binary(&self, owner: &str, space: &str, key: &str) -> StoreResult<bool> {
        Ok(self.connection.query_row("SELECT typeof(value)='blob' FROM plugin_device_storage WHERE owner=?1 AND space=?2 AND key=?3 AND tombstone=0",
            params![owner,space,key], |row| row.get(0)).optional()?.unwrap_or(false))
    }
    pub(crate) fn read_plugin_device_bytes(&self, owner: &str, space: &str, key: &str, version: &str, offset: u64) -> StoreResult<Vec<u8>> {
        let (rowid, length, current) = self.plugin_value_identity(owner, space, key)?.ok_or_else(|| invalid("PLUGIN_DEVICE_CHANGED"))?;
        if current != version || !self.plugin_value_is_binary(owner, space, key)? { return Err(invalid("PLUGIN_DEVICE_CHANGED")); }
        if offset >= length { return Err(invalid("plugin binary offset is invalid")); }
        let mut input = self.connection.blob_open(MAIN_DB, "plugin_device_storage", "value", rowid, true)?;
        input.seek(SeekFrom::Start(offset))?;
        let mut bytes = vec![0; (length - offset).min(crate::asset_byte_transport::BINARY_PART_BYTES as u64) as usize];
        input.read_exact(&mut bytes)?;
        Ok(bytes)
    }
}

#[cfg(test)]
pub(crate) fn fixture(payload: &[u8]) -> Vec<u8> {
    let metadata = format!(r#"[{{"type":"ArrayBuffer","byteLength":{}}}]"#, payload.len());
    let root = br#"{"ref":0}"#;
    let mut bytes = vec![0, 80, 76, 83, 1];
    bytes.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&(root.len() as u32).to_le_bytes());
    bytes.extend_from_slice(metadata.as_bytes());
    bytes.extend_from_slice(root);
    bytes.extend_from_slice(payload);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::{PluginDeviceMutation, PluginDeviceRead};

    #[test]
    fn binary_sessions_keep_exact_bytes_across_reopen_and_failed_writes() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = DeviceStore::open(directory.path()).unwrap();
        let bytes = fixture(&[0, 255, 128, 3]);
        let opened = store.open_plugin_device_binary_write("owner", "key", bytes.len() as u64).unwrap();
        assert!(store.append_plugin_device_write("owner", &opened.session_id, 0, "text").is_err());
        let opened = store.open_plugin_device_binary_write("owner", "key", bytes.len() as u64).unwrap();
        store.append_plugin_device_bytes(&opened.session_id, 0, &bytes).unwrap();
        assert!(store.write_plugin_device_values("other", &[PluginDeviceMutation::Staged { session_id: opened.session_id.clone() }]).is_err());
        store.write_plugin_device_values("owner", &[PluginDeviceMutation::Staged { session_id: opened.session_id }]).unwrap();
        drop(store);
        let mut store = DeviceStore::open(directory.path()).unwrap();
        let Some(PluginDeviceRead::Binary { version, byte_size, .. }) = store.read_plugin_device_transfer("owner", "json", "key").unwrap() else { panic!("expected binary value"); };
        assert_eq!(byte_size, bytes.len() as u64);
        assert_eq!(store.read_plugin_device_bytes("owner", "json", "key", &version, 0).unwrap(), bytes);
        let opened = store.open_plugin_device_binary_write("owner", "key", bytes.len() as u64).unwrap();
        store.append_plugin_device_bytes(&opened.session_id, 0, &bytes[..5]).unwrap();
        assert!(store.write_plugin_device_values("owner", &[PluginDeviceMutation::Staged { session_id: opened.session_id }]).is_err());
        assert_eq!(store.read_plugin_device_bytes("owner", "json", "key", &version, 0).unwrap(), bytes);
        store.write_plugin_device_values("owner", &[PluginDeviceMutation::Set { space: "json".into(), key: "key".into(), value: "42".into() }]).unwrap();
        assert!(store.read_plugin_device_bytes("owner", "json", "key", &version, 0).unwrap_err().to_string().contains("PLUGIN_DEVICE_CHANGED"));
    }
}
