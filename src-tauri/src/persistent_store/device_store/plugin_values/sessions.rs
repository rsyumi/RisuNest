use super::{text_value, validate_key, validate_owner, validate_space};
use super::super::{invalid, DeviceStore};
use crate::persistent_store::StoreResult;
use serde::Serialize;
use std::{collections::HashMap, fs::File, io::{Seek, SeekFrom, Write}, time::{Duration, Instant}};

const IDLE_LIFETIME: Duration = Duration::from_secs(5 * 60);
const OWNER_SESSIONS: usize = 2;
const TOTAL_SESSIONS: usize = 8;
const STAGING_BYTES: u64 = 2 * 1024 * 1024 * 1024;

pub(super) struct PendingWrite {
    pub(super) binary: bool,
    receiver: Option<std::sync::Arc<crate::asset_byte_transport::AndroidReceiver>>,
    pub(super) owner: String,
    pub(super) space: String,
    pub(super) key: String,
    pub(super) file: File,
    pub(super) byte_length: u64,
    encoded_length: u64,
    received_encoded: u64,
    touched: Instant,
}

#[derive(Default)]
pub(crate) struct WriteSessions { sessions: HashMap<String, PendingWrite> }

impl WriteSessions {
    pub(crate) fn expire(&mut self) { self.expire_at(Instant::now()); }
    fn expire_at(&mut self, now: Instant) {
        self.sessions.retain(|_, session| now.duration_since(session.touched) < IDLE_LIFETIME);
    }
    pub(super) fn take(&mut self, owner: &str, id: &str) -> StoreResult<PendingWrite> {
        self.expire();
        if !self.sessions.get(id).is_some_and(|session| session.owner == owner) {
            return Err(invalid("plugin device write session is unavailable"));
        }
        let mut session = self.sessions.remove(id).unwrap();
        if session.received_encoded != session.encoded_length {
            return Err(invalid("plugin device write session is incomplete"));
        }
        session.file.seek(SeekFrom::Start(0))?;
        Ok(session)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OpenedWrite { pub(crate) session_id: String }

impl DeviceStore {
    pub(crate) fn open_plugin_device_write(&mut self, owner: &str, space: &str, key: &str, encoded_length: u64) -> StoreResult<OpenedWrite> {
        self.open_plugin_device_write_with_limit(owner, space, key, encoded_length, text_value::MAX_VALUE_BYTES)
    }

    pub(super) fn open_plugin_device_write_with_limit(&mut self, owner: &str, space: &str, key: &str, encoded_length: u64, limit: u64) -> StoreResult<OpenedWrite> {
        validate_owner(owner)?; validate_space(space)?; validate_key(key)?;
        if encoded_length < 2 || encoded_length > limit { return Err(invalid("plugin device value is too large")); }
        self.plugin_writes.expire();
        let sessions = &mut self.plugin_writes.sessions;
        if sessions.len() >= TOTAL_SESSIONS || sessions.values().filter(|session| session.owner == owner).count() >= OWNER_SESSIONS
            || sessions.values().map(|session| session.encoded_length).sum::<u64>() + encoded_length > STAGING_BYTES {
            return Err(invalid("plugin device write sessions are full"));
        }
        let id = uuid::Uuid::new_v4().to_string();
        sessions.insert(id.clone(), PendingWrite { binary: false, receiver: None, owner: owner.into(), space: space.into(), key: key.into(),
            file: tempfile::tempfile()?, byte_length: 0, encoded_length, received_encoded: 2, touched: Instant::now() });
        Ok(OpenedWrite { session_id: id })
    }

    pub(crate) fn append_plugin_device_write(&mut self, owner: &str, id: &str, offset: u64, part: &str) -> StoreResult<u64> {
        self.plugin_writes.expire();
        let session = self.plugin_writes.sessions.get_mut(id).filter(|session| session.owner == owner)
            .ok_or_else(|| invalid("plugin device write session is unavailable"))?;
        let result = (|| {
            let encoded = text_value::encoded_length(part);
            if session.binary || part.is_empty() || encoded > text_value::PART_BYTES as u64 || offset != session.byte_length
                || session.received_encoded + encoded - 2 > session.encoded_length {
                return Err(invalid("plugin device write part is invalid"));
            }
            session.file.write_all(part.as_bytes())?;
            session.byte_length += part.len() as u64;
            session.received_encoded += encoded - 2;
            session.touched = Instant::now();
            Ok(session.byte_length)
        })();
        if result.is_err() { self.plugin_writes.sessions.remove(id); }
        result
    }

    pub(crate) fn abort_plugin_device_write(&mut self, owner: &str, id: &str) {
        if self.plugin_writes.sessions.get(id).is_some_and(|session| session.owner == owner) {
            self.plugin_writes.sessions.remove(id);
        }
    }

    pub(crate) fn open_plugin_device_binary_write(&mut self, owner: &str, key: &str, length: u64) -> StoreResult<OpenedWrite> {
        let opened = self.open_plugin_device_write(owner, "json", key, length)?;
        let session = self.plugin_writes.sessions.get_mut(&opened.session_id).unwrap();
        session.binary = true;
        session.received_encoded = 0;
        Ok(opened)
    }

    pub(crate) fn attach_plugin_binary_receiver(&mut self, id: &str, receiver: std::sync::Arc<crate::asset_byte_transport::AndroidReceiver>) -> StoreResult<()> {
        let session = self.plugin_writes.sessions.get_mut(id).ok_or_else(|| invalid("plugin device write session is unavailable"))?;
        session.receiver = Some(receiver);
        Ok(())
    }

    pub(crate) fn append_plugin_device_bytes(&mut self, id: &str, offset: u64, bytes: &[u8]) -> StoreResult<u64> {
        self.plugin_writes.expire();
        let session = self.plugin_writes.sessions.get_mut(id).ok_or_else(|| invalid("plugin device write session is unavailable"))?;
        let result = (|| {
            if !session.binary || bytes.is_empty() || bytes.len() > crate::asset_byte_transport::BINARY_PART_BYTES
                || offset != session.byte_length || offset + bytes.len() as u64 > session.encoded_length {
                return Err(invalid("plugin binary write part is invalid"));
            }
            session.file.write_all(bytes)?;
            session.byte_length += bytes.len() as u64;
            session.received_encoded = session.byte_length;
            session.touched = Instant::now();
            Ok(session.byte_length)
        })();
        if result.is_err() { self.plugin_writes.sessions.remove(id); }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::{PluginDeviceMutation, PluginDeviceRange};

    #[test]
    fn staged_values_commit_atomically_and_reject_incomplete_expired_or_oversized_writes() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = DeviceStore::open(directory.path()).unwrap();
        let set = |value: &str| PluginDeviceMutation::Set { space: "string".into(), key: "key".into(), value: value.into() };
        store.write_plugin_device_values("owner", &[set("before")]).unwrap();
        let previous = store.revision().unwrap();
        let value = "가😀\"x";
        let size = text_value::encoded_length(value);
        assert!(store.open_plugin_device_write_with_limit("owner", "string", "key", size, size - 1).is_err());
        let id = store.open_plugin_device_write_with_limit("owner", "string", "key", size, size).unwrap().session_id;
        store.append_plugin_device_write("owner", &id, 0, "가").unwrap();
        assert_eq!(store.read_plugin_device_value("owner", "string", "key").unwrap().as_deref(), Some("before"));
        assert!(store.write_plugin_device_values("owner", &[PluginDeviceMutation::Staged { session_id: id.clone() }]).is_err());
        assert_eq!(store.revision().unwrap(), previous);
        assert!(store.append_plugin_device_write("owner", &id, 3, "😀\"x").is_err());

        let id = store.open_plugin_device_write_with_limit("owner", "string", "key", size, size).unwrap().session_id;
        let offset = store.append_plugin_device_write("owner", &id, 0, "가").unwrap();
        store.append_plugin_device_write("owner", &id, offset, "😀\"x").unwrap();
        store.write_plugin_device_values("owner", &[PluginDeviceMutation::Staged { session_id: id }]).unwrap();
        assert_eq!(store.read_plugin_device_value("owner", "string", "key").unwrap().as_deref(), Some(value));
        let storage_type: String = store.connection.query_row("SELECT typeof(value) FROM plugin_device_storage WHERE owner='owner'", [], |row| row.get(0)).unwrap();
        assert_eq!(storage_type, "text");

        let (rowid, _, version) = store.plugin_value_identity("owner", "string", "key").unwrap().unwrap();
        assert_eq!(text_value::range(&store.connection, rowid, 0, 8).unwrap(), "가");
        assert_eq!(text_value::range(&store.connection, rowid, 3, 8).unwrap(), "😀\"");
        store.write_plugin_device_values("another-owner", &[set("unrelated")]).unwrap();
        assert!(matches!(store.read_plugin_device_range("owner", "string", "key", &version, 3).unwrap(),
            PluginDeviceRange::Value { value, .. } if value == "😀\"x"));
        store.write_plugin_device_values("owner", &[set("after")]).unwrap();
        assert!(matches!(store.read_plugin_device_range("owner", "string", "key", &version, 3).unwrap(), PluginDeviceRange::Changed));
        let previous = store.revision().unwrap();
        assert!(store.write_plugin_device_values_with_limits("owner", &[set(&"x".repeat(90)), set(&"y".repeat(90))], 128, 256).is_err());
        assert_eq!(store.revision().unwrap(), previous);
        assert_eq!(store.read_plugin_device_value("owner", "string", "key").unwrap().as_deref(), Some("after"));

        let id = store.open_plugin_device_write("owner", "string", "key", 6).unwrap().session_id;
        store.append_plugin_device_write("owner", &id, 0, "lost").unwrap();
        store.plugin_writes.expire_at(Instant::now() + IDLE_LIFETIME);
        assert!(store.write_plugin_device_values("owner", &[PluginDeviceMutation::Staged { session_id: id }]).is_err());
        assert_eq!(store.read_plugin_device_value("owner", "string", "key").unwrap().as_deref(), Some("after"));
    }
}
