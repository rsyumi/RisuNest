use super::{commit, PersistentStore, StoreError, StoreResult};
use rusqlite::{params, Connection, OptionalExtension, MAIN_DB};
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};
use std::fmt;
use std::io::{self, BufReader, BufWriter, Write};
use std::path::Path;
use std::sync::{atomic::{AtomicBool, Ordering}, Arc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FieldKind { Undefined, Value, Array, Object }

/// A disk-backed root preserves replacement and insertion order independently.
/// Collection members are decoded, normalized and emitted one at a time.
pub(crate) struct RootSpool {
    db: Connection,
    _file: Option<tempfile::NamedTempFile>,
    cancelled: Arc<AtomicBool>,
}

const MEMBERS_IN_ORDER: &str = "SELECT key,value FROM members WHERE field=?1 AND value IS NOT NULL
    ORDER BY array_index IS NULL,array_index,rowid";
// Reading in index order keeps the rowid order of object members, which is the
// order a copy is read back in, without sorting the values.
const MEMBERS_TO_COPY: &str = "SELECT key,value,array_index FROM members WHERE field=?1
    ORDER BY array_index IS NULL,array_index,rowid";
const INSERT_MEMBER: &str = "INSERT INTO members(field,key,value,array_index) VALUES(?1,?2,?3,?4)";

fn invalid(message: &str) -> StoreError {
    StoreError::Validation { message: message.to_owned() }
}

fn array_index(key: &str) -> Option<i64> {
    let index = key.parse::<u32>().ok()?;
    (index != u32::MAX && index.to_string() == key).then_some(i64::from(index))
}

fn decode(raw: &str) -> StoreResult<Value> {
    let mut decoder=serde_json::Deserializer::from_str(raw);
    decoder.disable_recursion_limit();
    let value=Value::deserialize(&mut decoder)?;
    decoder.end()?;
    Ok(value)
}

impl RootSpool {
    pub(crate) fn new(cancelled: Arc<AtomicBool>) -> StoreResult<Self> {
        Self::new_in(None,cancelled)
    }

    pub(crate) fn new_in(directory: Option<&Path>, cancelled: Arc<AtomicBool>) -> StoreResult<Self> {
        let file=directory.map(tempfile::NamedTempFile::new_in).transpose()?;
        // An empty filename asks SQLite to own a private, automatically removed
        // temporary database when no enclosing file job owns a scratch directory.
        let db=match &file { Some(file)=>crate::sqlite_open::open(file.path())?, None=>crate::sqlite_open::open("")? };
        db.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA cache_size=-2048;
            CREATE TABLE fields (key TEXT UNIQUE NOT NULL, kind INTEGER NOT NULL, value TEXT, array_index INTEGER);
            CREATE TABLE members (field TEXT NOT NULL, key TEXT NOT NULL, value TEXT, array_index INTEGER, UNIQUE(field,key));
            CREATE INDEX members_order ON members(field,array_index IS NULL,array_index);
            CREATE TABLE identities (scope TEXT NOT NULL, id TEXT NOT NULL, PRIMARY KEY(scope,id));")?;
        Ok(Self { db, _file: file, cancelled })
    }

    pub(crate) fn check_cancelled(&self) -> StoreResult<()> {
        if self.cancelled.load(Ordering::Acquire) { return Err(invalid("legacy root staging cancelled")); }
        Ok(())
    }

    pub(super) fn from_stored(connection: &Connection, repository_root: &Path, generation: &str) -> StoreResult<Self> {
        let scratch=crate::external_storage::leftovers::scratch_directory(repository_root)?;
        let root=Self::new_in(Some(&scratch),Arc::new(AtomicBool::new(false)))?;
        let rowid=connection.query_row("SELECT rowid FROM root WHERE generation=?1",[generation],|r|r.get(0))?;
        let blob=connection.blob_open(MAIN_DB,"root","value",rowid,true)?;
        let mut decoder=serde_json::Deserializer::from_reader(BufReader::with_capacity(64*1024,blob));
        decoder.disable_recursion_limit();
        StoredRootSeed(&root).deserialize(&mut decoder)?;
        decoder.end()?;
        Ok(root)
    }

    pub(super) fn record_by_id(&self, field: &str, id: &str) -> StoreResult<Option<Value>> {
        if self.kind(field)? != Some(FieldKind::Array) { return Ok(None); }
        let raw: Option<String>=self.db.query_row("SELECT value FROM members WHERE field=?1 AND json_extract(value,'$.id')=?2 ORDER BY rowid LIMIT 1",
            params![field,id],|r|r.get(0)).optional()?;
        raw.map(|raw|decode(&raw)).transpose()
    }

    pub(super) fn visit_value_texts(&self, mut visit: impl FnMut(&str) -> StoreResult<()>) -> StoreResult<()> {
        let mut statement=self.db.prepare("SELECT value FROM fields WHERE kind=1 UNION ALL SELECT value FROM members WHERE value IS NOT NULL")?;
        let mut rows=statement.query([])?;
        while let Some(row)=rows.next()? {
            self.check_cancelled()?;
            visit(row.get_ref(0)?.as_str().map_err(rusqlite::Error::from)?)?;
        }
        Ok(())
    }

    pub(crate) fn begin(&self, key: &str, kind: FieldKind) -> StoreResult<()> {
        self.check_cancelled()?;
        self.db.execute("DELETE FROM members WHERE field=?1", [key])?;
        self.db.execute("INSERT INTO fields(key,kind,array_index) VALUES(?1,?2,?3)
            ON CONFLICT(key) DO UPDATE SET kind=excluded.kind,value=NULL", params![key,kind as i64,array_index(key)])?;
        Ok(())
    }

    pub(crate) fn kind(&self, key: &str) -> StoreResult<Option<FieldKind>> {
        let kind: Option<i64> = self.db.query_row("SELECT kind FROM fields WHERE key=?1", [key], |r| r.get(0)).optional()?;
        Ok(match kind { Some(1) => Some(FieldKind::Value), Some(2) => Some(FieldKind::Array),
            Some(3) => Some(FieldKind::Object), _ => None })
    }

    pub(crate) fn finish_decode(&self) -> StoreResult<()> {
        self.db.execute("DELETE FROM fields WHERE kind=0",[])?;
        Ok(())
    }

    pub(crate) fn put(&self, key: &str, value: Option<&Value>) -> StoreResult<()> {
        match value {
            None => self.begin(key, FieldKind::Undefined)?,
            Some(Value::Array(values)) => {
                self.begin(key, FieldKind::Array)?;
                for (index,value) in values.iter().enumerate() { self.put_member(key,&index.to_string(),Some(value))?; }
            }
            Some(Value::Object(values)) => {
                self.begin(key, FieldKind::Object)?;
                for (name,value) in values { self.put_member(key,name,Some(value))?; }
            }
            Some(value) => {
                self.begin(key, FieldKind::Value)?;
                self.db.execute("UPDATE fields SET value=?2 WHERE key=?1",params![key,serde_json::to_string(value)?])?;
            }
        }
        Ok(())
    }

    pub(crate) fn put_member(&self, field: &str, key: &str, value: Option<&Value>) -> StoreResult<()> {
        self.check_cancelled()?;
        let raw = value.map(serde_json::to_string).transpose()?;
        self.db.execute("INSERT INTO members(field,key,value,array_index) VALUES(?1,?2,?3,?4)
            ON CONFLICT(field,key) DO UPDATE SET value=excluded.value",params![field,key,raw,array_index(key)])?;
        Ok(())
    }

    pub(crate) fn member(&self, field: &str, key: &str) -> StoreResult<Option<Value>> {
        let raw: Option<String> = self.db.query_row("SELECT value FROM members WHERE field=?1 AND key=?2 AND value IS NOT NULL",
            params![field,key],|row| row.get(0)).optional()?;
        raw.map(|raw| decode(&raw)).transpose()
    }

    pub(crate) fn scalar(&self, key: &str) -> StoreResult<Option<Value>> {
        if self.kind(key)? != Some(FieldKind::Value) { return Ok(None); }
        self.value(key)
    }

    pub(crate) fn rename_member(&self, field: &str, from: &str, to: &str) -> StoreResult<()> {
        self.db.execute("INSERT INTO members(field,key,value,array_index)
            SELECT field,?3,value,?4 FROM members WHERE field=?1 AND key=?2 AND value IS NOT NULL
            ON CONFLICT(field,key) DO UPDATE SET value=excluded.value",params![field,from,to,array_index(to)])?;
        self.db.execute("DELETE FROM members WHERE field=?1 AND key=?2",params![field,from])?;
        Ok(())
    }

    /// Members are read from `members_order` as the walk advances, so `visit`
    /// may replace or remove the member it is given but must not add members to `field`.
    pub(crate) fn visit(&self, field: &str, mut visit: impl FnMut(&str, Value) -> StoreResult<()>) -> StoreResult<()> {
        let mut statement = self.db.prepare(MEMBERS_IN_ORDER)?;
        let mut rows = statement.query([field])?;
        while let Some(row) = rows.next()? {
            self.check_cancelled()?;
            let key: String = row.get(0)?;
            let raw: String = row.get(1)?;
            visit(&key, decode(&raw)?)?;
        }
        Ok(())
    }

    pub(crate) fn count(&self, field: &str) -> StoreResult<u64> {
        let count: i64=self.db.query_row("SELECT COUNT(*) FROM members WHERE field=?1 AND value IS NOT NULL",[field],|r| r.get(0))?;
        u64::try_from(count).map_err(|_|invalid("legacy root member count is negative"))
    }

    #[cfg(test)]
    pub(crate) fn member_storage_shape(&self) -> StoreResult<(u64,u64)> {
        let (count,bytes): (i64,i64)=self.db.query_row("SELECT COUNT(*),COALESCE(MAX(length(CAST(value AS BLOB))),0) FROM members WHERE value IS NOT NULL",[],|row|Ok((row.get(0)?,row.get(1)?)))?;
        Ok((u64::try_from(count).map_err(|_|invalid("legacy root member count is negative"))?,
            u64::try_from(bytes).map_err(|_|invalid("legacy root member length is negative"))?))
    }

    pub(crate) fn remember_id(&self, scope: &str, id: &str) -> StoreResult<bool> {
        Ok(self.db.execute("INSERT OR IGNORE INTO identities(scope,id) VALUES(?1,?2)",params![scope,id])? != 0)
    }

    pub(crate) fn remove(&self, key: &str) -> StoreResult<()> {
        self.db.execute("DELETE FROM members WHERE field=?1",[key])?;
        self.db.execute("DELETE FROM fields WHERE key=?1",[key])?;
        Ok(())
    }

    pub(crate) fn remove_member(&self, field: &str, key: &str) -> StoreResult<()> {
        // Keep the cursor's ordering stable while filtering a collection.
        self.db.execute("UPDATE members SET value=NULL WHERE field=?1 AND key=?2",params![field,key])?;
        Ok(())
    }

    pub(crate) fn value(&self, key: &str) -> StoreResult<Option<Value>> {
        let Some(kind) = self.kind(key)? else { return Ok(None); };
        if kind == FieldKind::Value {
            let raw: String = self.db.query_row("SELECT value FROM fields WHERE key=?1",[key],|r|r.get(0))?;
            return Ok(Some(decode(&raw)?));
        }
        let mut array = Vec::new();
        let mut object = Map::new();
        self.visit(key, |name,value| {
            if kind == FieldKind::Array { array.push(value); } else { object.insert(name.to_owned(),value); }
            Ok(())
        })?;
        Ok(Some(if kind == FieldKind::Array { Value::Array(array) } else { Value::Object(object) }))
    }

    pub(crate) fn rename(&self, from: &str, to: &str) -> StoreResult<()> {
        let Some(kind) = self.kind(from)? else { return Ok(()); };
        self.begin(to,kind)?;
        self.db.execute("UPDATE fields SET value=(SELECT value FROM fields WHERE key=?2) WHERE key=?1",params![to,from])?;
        self.db.execute("UPDATE members SET field=?2 WHERE field=?1",params![from,to])?;
        self.remove(from)
    }

    fn copy_field(&self, from: &str, to: &str) -> StoreResult<()> {
        let Some(kind) = self.kind(from)? else { return Ok(()); };
        self.begin(to,kind)?;
        self.db.execute("UPDATE fields SET value=(SELECT value FROM fields WHERE key=?2) WHERE key=?1",params![to,from])?;
        // An INSERT that selects from its own table first copies every selected
        // value into a temporary table, so members are copied one row at a time.
        let mut insert=self.db.prepare(INSERT_MEMBER)?;
        let mut statement=self.db.prepare(MEMBERS_TO_COPY)?;
        let mut rows=statement.query([from])?;
        while let Some(row)=rows.next()? {
            self.check_cancelled()?;
            let column=|index: usize| row.get_ref(index).map(rusqlite::types::ToSqlOutput::Borrowed);
            insert.execute(params![to,column(0)?,column(1)?,column(2)?])?;
        }
        Ok(())
    }

    fn write_field(&self, key: &str, kind: FieldKind, writer: &mut impl Write) -> StoreResult<()> {
        if kind == FieldKind::Value {
            let raw: String = self.db.query_row("SELECT value FROM fields WHERE key=?1",[key],|r|r.get(0))?;
            writer.write_all(raw.as_bytes())?;
        } else {
            writer.write_all(if kind == FieldKind::Array { b"[" } else { b"{" })?;
            let mut first = true;
            let mut statement = self.db.prepare(MEMBERS_IN_ORDER)?;
            let mut rows = statement.query([key])?;
            while let Some(row) = rows.next()? {
                self.check_cancelled()?;
                if !first { writer.write_all(b",")?; }
                first = false;
                if kind == FieldKind::Object {
                    serde_json::to_writer(&mut *writer,&row.get::<_,String>(0)?)?;
                    writer.write_all(b":")?;
                }
                writer.write_all(row.get::<_,String>(1)?.as_bytes())?;
            }
            writer.write_all(if kind == FieldKind::Array { b"]" } else { b"}" })?;
        }
        Ok(())
    }

    pub(crate) fn write_root(&self, writer: &mut impl Write, separate_storage: bool) -> StoreResult<()> {
        writer.write_all(b"{")?;
        let mut first = true;
        let mut statement = self.db.prepare("SELECT key,kind FROM fields WHERE kind<>0 ORDER BY array_index IS NULL,array_index,rowid")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            self.check_cancelled()?;
            let key: String = row.get(0)?;
            if key == "botPresets" || key == "account" || key == "characters"
                || (separate_storage && matches!(key.as_str(),"pluginCustomStorage"|"pluginStorageMeta")) { continue; }
            if !first { writer.write_all(b",")?; }
            first = false;
            serde_json::to_writer(&mut *writer,&key)?;
            writer.write_all(b":")?;
            self.write_field(&key,self.kind(&key)?.ok_or_else(||invalid("root field disappeared"))?,writer)?;
        }
        writer.write_all(b"}")?;
        Ok(())
    }

    fn assign_record_id(&self, scope: &str, record: &mut Value) -> StoreResult<()> {
        let object = record.as_object_mut().ok_or_else(||invalid("Imported record must be an object"))?;
        let id = object.get("id").and_then(Value::as_str).filter(|id|!id.is_empty());
        let id = match id {
            Some(id) if self.remember_id(scope,id)? => id.to_owned(),
            _ => { let id=uuid::Uuid::new_v4().to_string(); self.remember_id(scope,&id)?; id }
        };
        object.insert("id".into(),Value::String(id));
        Ok(())
    }

    fn selected(&self, field: &str, selector: &str) -> StoreResult<Option<String>> {
        let count = self.count(field)?;
        if count == 0 { return Ok(None); }
        let selector = self.scalar(selector)?;
        if let Some(Value::String(id)) = &selector {
            let found: Option<String> = self.db.query_row("SELECT key FROM members WHERE field=?1 AND json_extract(value,'$.id')=?2 ORDER BY rowid LIMIT 1",
                params![field,id],|r|r.get(0)).optional()?;
            if found.is_some() { return Ok(found); }
        }
        Ok(Some(selector.as_ref().and_then(Value::as_u64).unwrap_or(0).min(count-1).to_string()))
    }

    fn normalize(&self) -> StoreResult<()> {
        for field in ["modules","loadouts","customModels","personas","botPresets"] {
            if self.kind(field)? != Some(FieldKind::Array) { continue; }
            self.visit(field,|key,mut record| {
                self.assign_record_id(field,&mut record)?;
                if field == "personas" {
                    if let Some(module) = record.get_mut("embeddedModule") {
                        let object = module.as_object_mut().ok_or_else(||invalid("Imported record must be an object"))?;
                        if object.get("id").and_then(Value::as_str).is_none_or(str::is_empty) {
                            object.insert("id".into(),Value::String(uuid::Uuid::new_v4().to_string()));
                        }
                    }
                }
                self.put_member(field,key,Some(&record))
            })?;
        }
        if self.kind("personas")? == Some(FieldKind::Array) {
            if let Some(key) = self.selected("personas","selectedPersona")? {
                let mut persona = self.member("personas",&key)?.ok_or_else(||invalid("selected persona is missing"))?;
                for (key,field) in [("username","name"),("userIcon","icon"),("personaPrompt","personaPrompt"),("userNote","note")] {
                    if let Some(value) = self.value(key)? { persona[field]=value; }
                }
                self.put("selectedPersona",Some(&persona["id"]))?;
                self.put_member("personas",&key,Some(&persona))?;
            }
        }
        if self.kind("explicitGlobalChatVariables")?.is_none() { self.copy_field("globalChatVariables","explicitGlobalChatVariables")?; }
        if let Some(key) = self.selected("botPresets","botPresetsId")? {
            let mut preset = self.member("botPresets",&key)?.ok_or_else(||invalid("selected preset is missing"))?;
            for (key,field) in super::export::PRESET_MIRRORS {
                if let Some(value) = self.value(key)? {
                    let protected = super::export::protected_preset_flag(key)
                        .map(|flag|self.scalar(flag)).transpose()?.flatten().and_then(|v|v.as_bool()).unwrap_or(false);
                    if protected {
                        if self.kind("protectedPresetValues")? != Some(FieldKind::Object) { self.begin("protectedPresetValues",FieldKind::Object)?; }
                        self.put_member("protectedPresetValues",key,Some(&value))?;
                    } else { preset[*field]=value; }
                }
            }
            self.put("botPresetsId",Some(&preset["id"]))?;
            self.put_member("botPresets",&key,Some(&preset))?;
        }
        Ok(())
    }
}

struct StoredRootSeed<'a>(&'a RootSpool);
impl<'de> DeserializeSeed<'de> for StoredRootSeed<'_> {
    type Value=();
    fn deserialize<D: Deserializer<'de>>(self, decoder:D)->Result<(),D::Error> { decoder.deserialize_map(self) }
}
impl<'de> Visitor<'de> for StoredRootSeed<'_> {
    type Value=();
    fn expecting(&self, f:&mut fmt::Formatter<'_>)->fmt::Result { f.write_str("a persistent root object") }
    fn visit_map<A:MapAccess<'de>>(self,mut map:A)->Result<(),A::Error> {
        while let Some(key)=map.next_key::<String>()? {
            map.next_value_seed(StoredFieldSeed { root:self.0,key:&key })?;
        }
        Ok(())
    }
}

struct StoredFieldSeed<'a> { root:&'a RootSpool,key:&'a str }
impl<'de> DeserializeSeed<'de> for StoredFieldSeed<'_> {
    type Value=();
    fn deserialize<D:Deserializer<'de>>(self,decoder:D)->Result<(),D::Error> { decoder.deserialize_any(self) }
}
impl StoredFieldSeed<'_> {
    fn scalar<E:de::Error>(self,value:Value)->Result<(),E> { self.root.put(self.key,Some(&value)).map_err(E::custom) }
}
impl<'de> Visitor<'de> for StoredFieldSeed<'_> {
    type Value=();
    fn expecting(&self,f:&mut fmt::Formatter<'_>)->fmt::Result { f.write_str("a root field") }
    fn visit_unit<E:de::Error>(self)->Result<(),E> { self.scalar(Value::Null) }
    fn visit_bool<E:de::Error>(self,value:bool)->Result<(),E> { self.scalar(Value::Bool(value)) }
    fn visit_i64<E:de::Error>(self,value:i64)->Result<(),E> { self.scalar(Value::from(value)) }
    fn visit_u64<E:de::Error>(self,value:u64)->Result<(),E> { self.scalar(Value::from(value)) }
    fn visit_f64<E:de::Error>(self,value:f64)->Result<(),E> { self.scalar(Value::from(value)) }
    fn visit_str<E:de::Error>(self,value:&str)->Result<(),E> { self.scalar(Value::String(value.to_owned())) }
    fn visit_string<E:de::Error>(self,value:String)->Result<(),E> { self.scalar(Value::String(value)) }
    fn visit_seq<A:SeqAccess<'de>>(self,mut sequence:A)->Result<(),A::Error> {
        self.root.begin(self.key,FieldKind::Array).map_err(de::Error::custom)?;
        let mut index=0u64;
        while let Some(value)=sequence.next_element::<Value>()? {
            self.root.put_member(self.key,&index.to_string(),Some(&value)).map_err(de::Error::custom)?;
            index+=1;
        }
        Ok(())
    }
    fn visit_map<A:MapAccess<'de>>(self,mut map:A)->Result<(),A::Error> {
        self.root.begin(self.key,FieldKind::Object).map_err(de::Error::custom)?;
        while let Some((key,value))=map.next_entry::<String,Value>()? {
            self.root.put_member(self.key,&key,Some(&value)).map_err(de::Error::custom)?;
        }
        Ok(())
    }
}

impl PersistentStore {
    pub(crate) fn replace_put_upstream_stream(&mut self, staging_id: &str, root: &RootSpool) -> StoreResult<()> {
        commit::require_staging(&self.connection,staging_id)?;
        if root.kind("pluginCustomStorage")?.is_some_and(|kind|kind != FieldKind::Object) {
            return Err(invalid("pluginCustomStorage must be a JSON object"));
        }
        if root.kind("botPresets")?.is_some_and(|kind|kind != FieldKind::Array) {
            return Err(invalid("Imported presets must be an array"));
        }
        root.normalize()?;
        struct Length(u64);
        impl Write for Length {
            fn write(&mut self,bytes:&[u8])->io::Result<usize> { self.0+=bytes.len() as u64; Ok(bytes.len()) }
            fn flush(&mut self)->io::Result<()> { Ok(()) }
        }
        let mut length=Length(0);
        root.write_root(&mut length,true)?;
        let length=i32::try_from(length.0).map_err(|_|invalid("legacy root exceeds SQLite value limit"))?;
        let tx=self.connection.transaction()?;
        commit::require_staging(&tx,staging_id)?;
        tx.execute("DELETE FROM bot_presets WHERE generation=?1",[staging_id])?;
        let mut preset_index=0;
        root.visit("botPresets",|_,preset| {
            commit::put_preset_rows(&tx,staging_id,&[preset],preset_index)?;
            preset_index+=1;
            Ok(())
        })?;
        tx.execute("DELETE FROM plugin_storage WHERE generation=?1",[staging_id])?;
        let carried=commit::carried_plugin_import_batches(&tx)?;
        let mut ordinal=0;
        root.visit("pluginCustomStorage",|key,value| {
            let mut values=Map::new(); values.insert(key.to_owned(),value);
            let mut meta=Map::new();
            if root.kind("pluginStorageMeta")? == Some(FieldKind::Object) {
                if let Some(value)=root.member("pluginStorageMeta",key)? { meta.insert(key.to_owned(),value); }
            }
            commit::put_plugin_storage_map(&tx,staging_id,&values,Some(&meta),&carried,ordinal)?;
            ordinal+=1;
            Ok(())
        })?;
        // TEXT is kept throughout: SQLite's incremental writer accepts both TEXT
        // and BLOB cells, and readers continue to receive the existing root type.
        tx.execute("UPDATE root SET value=CAST(zeroblob(?2) AS TEXT) WHERE generation=?1",params![staging_id,length])?;
        let rowid=tx.query_row("SELECT rowid FROM root WHERE generation=?1",[staging_id],|r|r.get(0))?;
        let mut blob=tx.blob_open(MAIN_DB,"root","value",rowid,false)?;
        {
            let mut writer=BufWriter::with_capacity(64*1024,&mut blob);
            root.write_root(&mut writer,true)?;
            writer.flush()?;
        }
        blob.close()?;
        root.check_cancelled()?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spool(value: &Value) -> RootSpool {
        let root=RootSpool::new(Arc::new(AtomicBool::new(false))).unwrap();
        for (key,value) in value.as_object().unwrap() { root.put(key,Some(value)).unwrap(); }
        root
    }

    #[test]
    fn streamed_root_matches_upstream_identity_mirrors_and_plugin_ownership() {
        let fixture=json!({
            "botPresetsId":1,"selectedPersona":1,"temperature":0.8,"NAIsettings":{"synthetic":true},
            "username":"edited","doNotChangeFallbackModels":true,"fallbackModels":["protected"],
            "personas":[{"id":"persona-one","name":"one"},{"id":"persona-two","name":"stale","embeddedModule":{"id":"embedded","assets":[]}}],
            "modules":[{"id":"module"}],"loadouts":[{"id":"loadout"}],"customModels":[{"id":"model"}],
            "plugins":[],"globalChatVariables":{"toggle_synthetic":"1","ordinary":"2"},
            "botPresets":[{"id":"preset-one","name":"one"},{"id":"preset-two","temperature":0.1,"fallbackModels":["preset"]}],
            "pluginCustomStorage":{"shared":{"synthetic":true},"unowned":"value"},
            "pluginStorageMeta":{"shared":{"plugin":"synthetic-plugin"}}
        });
        let first=tempfile::tempdir().unwrap(); let second=tempfile::tempdir().unwrap();
        let mut old=PersistentStore::open(first.path()).unwrap(); let mut streamed=PersistentStore::open(second.path()).unwrap();
        let before=streamed.read_root(None).unwrap().value;
        let a=old.replace_begin().unwrap().staging_id; let b=streamed.replace_begin().unwrap().staging_id;
        old.replace_put_upstream_root(&a,&fixture).unwrap();
        old.replace_put_upstream_presets(&a,fixture["botPresets"].as_array().unwrap()).unwrap();
        let root=spool(&fixture);
        streamed.replace_put_upstream_stream(&b,&root).unwrap();
        assert_eq!(streamed.materialize_staging(&b).unwrap(),old.materialize_staging(&a).unwrap());
        let (kind,valid):(String,bool)=streamed.connection.query_row("SELECT typeof(value),json_valid(value) FROM root WHERE generation=?1",[&b],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(kind,"text"); assert!(valid);
        assert_eq!(streamed.read_root(None).unwrap().value,before);
        assert_eq!(streamed.revision().unwrap(),0);
        let replay=RootSpool::from_stored(&streamed.connection,second.path(),&b).unwrap();
        assert_eq!(replay.record_by_id("personas","persona-two").unwrap().unwrap()["name"],"edited");
        let scratch=replay._file.as_ref().unwrap().path().to_path_buf();
        assert!(scratch.starts_with(second.path().join("external-storage").join("scratch")),"{scratch:?}");
        drop(replay);
        assert!(!scratch.exists());
        streamed.replace_commit(&b,Some(0)).unwrap();
        assert_eq!(streamed.revision().unwrap(),1);
        assert_eq!(streamed.materialize(None).unwrap()["temperature"],0.8);
    }

    #[test]
    fn member_reads_keep_member_order_without_sorting_values() {
        let root=RootSpool::new(Arc::new(AtomicBool::new(false))).unwrap();
        root.begin("object",FieldKind::Object).unwrap();
        for key in ["b","10","removed","2","4294967295","01","0"] { root.put_member("object",key,Some(&json!(key))).unwrap(); }
        root.remove_member("object","removed").unwrap();
        let mut keys=Vec::new();
        root.visit("object",|key,_| { keys.push(key.to_owned()); Ok(()) }).unwrap();
        assert_eq!(keys,["0","2","10","b","4294967295","01"]);
        root.copy_field("object","copy").unwrap();
        let mut text=Vec::new();
        root.write_root(&mut text,false).unwrap();
        let members=r#"{"0":"0","2":"2","10":"10","b":"b","4294967295":"4294967295","01":"01"}"#;
        assert_eq!(String::from_utf8(text).unwrap(),format!(r#"{{"object":{members},"copy":{members}}}"#));
        // A sorter or a temporary table would hold every member value, in RAM under
        // Android's SQLITE_TEMP_STORE=3.
        for sql in [MEMBERS_IN_ORDER,MEMBERS_TO_COPY,INSERT_MEMBER] {
            let mut statement=root.db.prepare(&format!("EXPLAIN {sql}")).unwrap();
            let unbound=vec![rusqlite::types::Null;statement.parameter_count()];
            let opcodes=statement.query_map(rusqlite::params_from_iter(unbound),|row| row.get::<_,String>(1)).unwrap()
                .collect::<Result<Vec<_>,_>>().unwrap();
            assert!(!opcodes.iter().any(|opcode| opcode=="SorterOpen" || opcode=="OpenEphemeral"),"{sql}: {opcodes:?}");
        }
    }

    #[test]
    fn streamed_root_assigns_record_ids_and_preserves_generated_selection() {
        let directory=tempfile::tempdir().unwrap();
        let mut store=PersistentStore::open(directory.path()).unwrap();
        let stage=store.replace_begin().unwrap().staging_id;
        let root=spool(&json!({"botPresetsId":2,"selectedPersona":1,"username":"selected", "modules":[{"id":"kept"},{"id":"kept"},{}],
            "personas":[{"id":"persona"},{"embeddedModule":{}}],"botPresets":[{"id":"preset"},{"id":"preset"},{}]}));
        store.replace_put_upstream_stream(&stage,&root).unwrap();
        let materialized=store.materialize_staging(&stage).unwrap();
        for field in ["modules","personas","botPresets"] {
            let records=materialized[field].as_array().unwrap();
            let ids=records.iter().map(|record|record["id"].as_str().unwrap()).collect::<std::collections::HashSet<_>>();
            assert_eq!(ids.len(),records.len());
            for record in &records[1..] { assert!(uuid::Uuid::parse_str(record["id"].as_str().unwrap()).is_ok()); }
        }
        assert_eq!(materialized["selectedPersona"],materialized["personas"][1]["id"]);
        assert_eq!(materialized["botPresetsId"],materialized["botPresets"][2]["id"]);
        assert_eq!(materialized["personas"][1]["name"],"selected");
        assert!(uuid::Uuid::parse_str(materialized["personas"][1]["embeddedModule"]["id"].as_str().unwrap()).is_ok());
    }

    #[test]
    fn cancelled_streamed_root_keeps_active_generation_and_removes_scratch_on_drop() {
        let directory=tempfile::tempdir().unwrap();
        let mut store=PersistentStore::open(directory.path()).unwrap();
        let before=store.read_root(None).unwrap().value;
        let stage=store.replace_begin().unwrap().staging_id;
        let root=RootSpool::new_in(Some(directory.path()),Arc::new(AtomicBool::new(false))).unwrap();
        root.put("modules",Some(&json!([{"id":"synthetic"}]))).unwrap();
        let scratch=root._file.as_ref().unwrap().path().to_path_buf();
        root.cancelled.store(true,Ordering::Release);
        assert!(store.replace_put_upstream_stream(&stage,&root).is_err());
        store.replace_abort(&stage).unwrap();
        assert_eq!(store.read_root(None).unwrap().value,before);
        assert_eq!(store.revision().unwrap(),0);
        drop(root);
        assert!(!scratch.exists());
    }

    #[test]
    fn streamed_root_write_failure_rolls_back_presets_and_plugin_stage_rows() {
        let directory=tempfile::tempdir().unwrap();
        let mut store=PersistentStore::open(directory.path()).unwrap();
        let before=store.read_root(None).unwrap().value;
        let stage=store.replace_begin().unwrap().staging_id;
        let root=spool(&json!({"modules":[],"botPresets":[{"id":"preset"}],"pluginCustomStorage":{"key":"synthetic"}}));
        store.connection.execute_batch("CREATE TEMP TRIGGER fail_streamed_root BEFORE UPDATE ON root BEGIN SELECT RAISE(ABORT,'synthetic root failure'); END;").unwrap();
        assert!(store.replace_put_upstream_stream(&stage,&root).is_err());
        for table in ["bot_presets","plugin_storage"] {
            let count:i64=store.connection.query_row(&format!("SELECT COUNT(*) FROM {table} WHERE generation=?1"),[&stage],|r|r.get(0)).unwrap();
            assert_eq!(count,0);
        }
        let staged:String=store.connection.query_row("SELECT value FROM root WHERE generation=?1",[&stage],|r|r.get(0)).unwrap();
        assert_eq!(staged,"{}");
        assert_eq!(store.read_root(None).unwrap().value,before);
        assert_eq!(store.revision().unwrap(),0);
        store.replace_abort(&stage).unwrap();
    }

    #[test]
    fn root_heavy_stage_preserves_plugin_preview_order_without_root_materialization() {
        let directory=tempfile::tempdir().unwrap();
        let mut store=PersistentStore::open(directory.path()).unwrap();
        let stage=store.replace_begin().unwrap().staging_id;
        let root=RootSpool::new(Arc::new(AtomicBool::new(false))).unwrap();
        for field in ["modules","plugins","botPresets"] { root.begin(field,FieldKind::Array).unwrap(); }
        for index in 0..128 {
            let body="synthetic 한글🙂".repeat(1024);
            root.put_member("modules",&index.to_string(),Some(&json!({"id":format!("module-{index}"),"body":body,"assets":[]}))).unwrap();
            root.put_member("plugins",&index.to_string(),Some(&json!({"name":format!("plugin-{index}"),"body":body}))).unwrap();
            root.put_member("botPresets",&index.to_string(),Some(&json!({"id":format!("preset-{index}"),"body":body}))).unwrap();
        }
        store.replace_put_upstream_stream(&stage,&root).unwrap();
        let scratch=directory.path().join("external-storage").join("scratch");
        std::fs::create_dir_all(scratch.parent().unwrap()).unwrap();
        if scratch.is_dir() { std::fs::remove_dir(&scratch).unwrap(); }
        std::fs::write(&scratch,b"synthetic unavailable scratch").unwrap();
        assert!(store.replace_preserve_repositories(&stage,Some(0)).is_err());
        assert!(commit::staged_plugin_preview(&store.connection,directory.path(),&stage).is_err());
        std::fs::remove_file(&scratch).unwrap();
        store.replace_preserve_repositories(&stage,Some(0)).unwrap();
        let preview=commit::staged_plugin_preview(&store.connection,directory.path(),&stage).unwrap();
        assert_eq!(std::fs::read_dir(&scratch).unwrap().count(),0);
        assert_eq!(preview.plugin_names,(0..128).map(|index|format!("plugin-{index}")).collect::<Vec<_>>());
        assert!(preview.values.is_empty());
        let root_bytes:i64=store.connection.query_row("SELECT length(CAST(value AS BLOB)) FROM root WHERE generation=?1",[&stage],|r|r.get(0)).unwrap();
        assert!(root_bytes>4*1024*1024);
        assert!(root.member_storage_shape().unwrap().1<64*1024);
        assert_eq!(store.revision().unwrap(),0);
        store.replace_abort(&stage).unwrap();
    }

    #[test]
    fn streamed_plugin_preview_keeps_scalar_metadata_edge_behavior() {
        let directory=tempfile::tempdir().unwrap();
        let mut store=PersistentStore::open(directory.path()).unwrap();
        let stage=store.replace_begin().unwrap().staging_id;
        for (value,names) in [
            (json!(null),vec![]), (json!({"ignored":"shape"}),vec![]),
            (json!("invalid"),vec![]),
            (json!("[{\"name\":\"Synthetic\"},{\"name\":\"\"},{\"name\":\"Synthetic\"}]"),vec!["Synthetic","Synthetic"]),
        ] {
            store.replace_put_root(&stage,&json!({"plugins":value})).unwrap();
            assert_eq!(commit::staged_plugin_preview(&store.connection,directory.path(),&stage).unwrap().plugin_names,names);
        }
        for value in [json!(true),json!(7)] {
            store.replace_put_root(&stage,&json!({"plugins":value})).unwrap();
            assert!(commit::staged_plugin_preview(&store.connection,directory.path(),&stage).is_err());
        }
    }
}
