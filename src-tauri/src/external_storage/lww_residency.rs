use super::{
    connection_store::{ConnectionStore, StoredConnection},
    contract::*,
    lww_segment::{self as segment, LargeBody},
};
use risunest_external_storage_format::snapshot::StoredObject;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use futures::StreamExt;
use std::{
    borrow::Borrow,
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Source {
    pub hash: String,
    pub library_id: String,
    pub connection_id: String,
    pub connection_root: PathBuf,
    pub protected_segment: String,
    pub body: LargeBody,
}
#[derive(Clone,Serialize,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub(crate) struct PackedSource<O=StoredObject> {
    pub hash:String,
    pub byte_length:u64,
    pub library_id:String,
    pub connection_id:String,
    pub connection_root:PathBuf,
    pub protected_snapshot:String,
    pub catalog:O,
    pub chunks:Vec<risunest_external_storage_format::snapshot::StoredChunk>,
    pub packs:Vec<O>,
}
/// A packed source whose catalog and packs are shared with the other sources
/// interned alongside it.
pub(crate) type SharedPackedSource=PackedSource<SharedObject>;
/// A stored object held once however many sources name it. It is written
/// exactly as the object itself.
#[derive(Clone,Debug,PartialEq,Eq)]
pub(crate) struct SharedObject(Arc<StoredObject>);
impl std::ops::Deref for SharedObject {
    type Target=StoredObject;
    fn deref(&self)->&StoredObject {&self.0}
}
impl Borrow<StoredObject> for SharedObject {
    fn borrow(&self)->&StoredObject {&self.0}
}
impl Serialize for SharedObject {
    fn serialize<S:serde::Serializer>(&self,serializer:S)->std::result::Result<S::Ok,S::Error> {self.0.serialize(serializer)}
}
impl<'de> Deserialize<'de> for SharedObject {
    fn deserialize<D:serde::Deserializer<'de>>(deserializer:D)->std::result::Result<Self,D::Error> {
        StoredObject::deserialize(deserializer).map(|object|Self(Arc::new(object)))
    }
}
/// Hands out one shared copy of each stored object, by object identity. A
/// different object under a known identity stays a copy of its own, so the
/// validation that follows still sees the conflict.
#[derive(Default)]
pub(crate) struct ObjectInterner(BTreeMap<String,SharedObject>);
impl ObjectInterner {
    pub(crate) fn intern(&mut self,object:StoredObject)->SharedObject {
        match self.0.get(&object.header.object_id) {
            Some(shared) if **shared==object=>shared.clone(),
            Some(_)=>SharedObject(Arc::new(object)),
            None=>{
                let shared=SharedObject(Arc::new(object));
                self.0.insert(shared.header.object_id.clone(),shared.clone());
                shared
            }
        }
    }
}
impl PackedSource {
    pub(crate) fn interned(self,interner:&mut ObjectInterner)->SharedPackedSource {
        let PackedSource{hash,byte_length,library_id,connection_id,connection_root,protected_snapshot,catalog,chunks,packs}=self;
        PackedSource{hash,byte_length,library_id,connection_id,connection_root,protected_snapshot,catalog:interner.intern(catalog),chunks,
            packs:packs.into_iter().map(|pack|interner.intern(pack)).collect()}
    }
}
impl SharedPackedSource {
    pub(crate) fn unshared(&self)->PackedSource {
        PackedSource{hash:self.hash.clone(),byte_length:self.byte_length,library_id:self.library_id.clone(),connection_id:self.connection_id.clone(),
            connection_root:self.connection_root.clone(),protected_snapshot:self.protected_snapshot.clone(),catalog:StoredObject::clone(&self.catalog),
            chunks:self.chunks.clone(),packs:self.packs.iter().map(|pack|StoredObject::clone(pack)).collect()}
    }
}
#[derive(Clone,Serialize,Deserialize)]
#[serde(tag="kind",rename_all="kebab-case",deny_unknown_fields)]
pub(crate) enum FrozenBodySource {
    Standalone(Source),
    Packed(PackedSource),
}
/// The connection root a registered source is read with. A source stores the
/// data root of the run that registered it, and a link at the data root can
/// have moved the files since. A stored root that still names the registry's
/// directory, and is not itself a link, is kept as written; any other is
/// replaced by the registry's root.
struct Rooting {
    root:PathBuf,
    canonical:Option<Option<PathBuf>>,
    kept:BTreeMap<PathBuf,bool>,
}
impl Rooting {
    fn new(root:&Path)->Self {Self{root:root.to_owned(),canonical:None,kept:BTreeMap::new()}}
    fn root(&mut self,stored:PathBuf)->PathBuf {
        if stored==self.root {return stored;}
        let kept=match self.kept.get(&stored) {
            Some(&kept)=>kept,
            None=>{
                let canonical=self.canonical.get_or_insert_with(||std::fs::canonicalize(&self.root).ok());
                let kept=canonical.is_some() && std::fs::canonicalize(&stored).ok()==*canonical
                    && !std::fs::symlink_metadata(&stored).is_ok_and(|metadata|crate::trust_boundary::is_link_like(&metadata));
                self.kept.insert(stored.clone(),kept);
                kept
            }
        };
        if kept {stored} else {self.root.clone()}
    }
}
impl Source {
    fn rooted(mut self,rooting:&mut Rooting)->Self {self.connection_root=rooting.root(std::mem::take(&mut self.connection_root));self}
}
impl<O> PackedSource<O> {
    fn rooted(mut self,rooting:&mut Rooting)->Self {self.connection_root=rooting.root(std::mem::take(&mut self.connection_root));self}
}
impl FrozenBodySource {
    /// The same source, read with `root` as the registry's root.
    pub(crate) fn rooted(self,root:&Path)->Self {self.rooted_by(&mut Rooting::new(root))}
    fn rooted_by(self,rooting:&mut Rooting)->Self {
        match self {
            Self::Standalone(source)=>Self::Standalone(source.rooted(rooting)),
            Self::Packed(source)=>Self::Packed(source.rooted(rooting)),
        }
    }
    pub(crate) fn byte_length(&self)->u64 {
        match self {
            Self::Standalone(source)=>source.body.plaintext_byte_length.0,
            Self::Packed(source)=>source.byte_length,
        }
    }
}
pub(crate) fn freeze_remote_body(root:&Path,hash:&str)->Result<Option<FrozenBodySource>> {
    RemoteBodies::connect(root)?.frozen(hash)
}
fn local(error: impl std::fmt::Display) -> ProviderError {
    segment::corrupt().caused(&error)
}
fn database(root: &Path, create: bool) -> Result<Option<Connection>> {
    let directory = root.join("external-sync");
    let path = directory.join("remote-bodies.sqlite");
    if !create && !path.exists() {
        return Ok(None);
    }
    std::fs::create_dir_all(&directory).map_err(local)?;
    if crate::trust_boundary::is_link_like(&std::fs::symlink_metadata(&directory).map_err(local)?) {
        return Err(segment::corrupt());
    }
    for suffix in ["", "-wal", "-shm"] {
        let path = PathBuf::from(format!("{}{suffix}", path.display()));
        if path.exists() {
            crate::trust_boundary::open_regular_source(&path).map_err(local)?;
        }
    }
    #[cfg(test)]
    hydration_tests::observe("remote-bodies", root, &path);
    let db = crate::sqlite_open::open(path).map_err(local)?;
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA busy_timeout=5000;
        CREATE TABLE IF NOT EXISTS sources(hash TEXT NOT NULL,library TEXT NOT NULL,source TEXT NOT NULL,PRIMARY KEY(hash,library));
        CREATE TABLE IF NOT EXISTS packed_sources(hash TEXT NOT NULL,library TEXT NOT NULL,source TEXT NOT NULL,PRIMARY KEY(hash,library));").map_err(local)?;
    Ok(Some(db))
}
/// One connection to the remote body registry, for a pass that asks about many
/// hashes.
pub(crate) struct RemoteBodies {
    rooting:Rooting,
    db:Option<Connection>,
}

pub(super) fn usage_receipts(root: &Path, connection: &StoredConnection) -> Result<Vec<ObjectReceipt>> {
    let Some(db) = database(root, false)? else { return Ok(Vec::new()); };
    let mut receipts = Vec::new();
    let format_repository_id = segment::format_repository_id(&connection.provider_repository_id);
    let mut bodies = db.prepare("SELECT DISTINCT json_extract(source,'$.body') FROM sources
        WHERE library=?1 AND json_extract(source,'$.connectionId')=?2").map_err(local)?;
    let rows = bodies.query_map(params![connection.descriptor.repository_id, connection.id], |row| row.get::<_, String>(0)).map_err(local)?;
    for row in rows {
        let body: LargeBody = serde_json::from_str(&row.map_err(local)?).map_err(local)?;
        let locator = body.locator.ok_or_else(segment::corrupt)?;
        if locator.connection_identity != connection.descriptor_locator.connection_identity { continue; }
        receipts.push(ObjectReceipt { locator, byte_length: body.byte_length.0, version: None, checksum: None, complete: true });
    }
    // A catalog or pack can be shared by thousands of assets. Decode each file once.
    let mut packed = db.prepare("SELECT json_extract(source,'$.catalog') FROM packed_sources
        WHERE library=?1 AND json_extract(source,'$.connectionId')=?2
        UNION SELECT member.value FROM packed_sources,json_each(source,'$.packs') member
        WHERE library=?1 AND json_extract(source,'$.connectionId')=?2").map_err(local)?;
    let rows = packed.query_map(params![connection.descriptor.repository_id, connection.id], |row| row.get::<_, String>(0)).map_err(local)?;
    for row in rows {
        let object: StoredObject = serde_json::from_str(&row.map_err(local)?).map_err(local)?;
        object.validate().map_err(local)?;
        if object.locator.connection_identity != connection.descriptor_locator.connection_identity
            || object.header.repository_id != format_repository_id { continue; }
        receipts.push(ObjectReceipt {
            locator: RemoteLocator { connection_identity: object.locator.connection_identity,
                collection: object.locator.collection, object: object.locator.object },
            byte_length: object.ciphertext_length, version: None, checksum: None, complete: true,
        });
    }
    Ok(receipts)
}
/// Hashes one registry query asks about.
const LOOKUP_PAGE:usize=512;
type ConnectionKey=(PathBuf,String,String);
/// A hash's registered source, with a packed one cut down to what grouping
/// needs; the full source is read again for its group.
enum Registered {
    Standalone(Source),
    Packed(ConnectionKey,PackedOutline),
}
#[derive(Deserialize)]
#[serde(rename_all="camelCase")]
struct OutlineRow {
    byte_length:u64,
    library_id:String,
    connection_id:String,
    connection_root:PathBuf,
    chunks:Vec<OutlineChunk>,
}
#[derive(Deserialize)]
#[serde(rename_all="camelCase")]
struct OutlineChunk {
    pack_id:String,
}
struct PackedOutline {
    hash:String,
    byte_length:u64,
    packs:Vec<String>,
}
/// Every connection that registered a source for a body, by connection root
/// and ID, with the length the source `frozen` would choose records.
pub(crate) struct BodyHolders {
    pub byte_length:u64,
    pub connections:BTreeSet<(PathBuf,String)>,
}
impl RemoteBodies {
    pub(crate) fn open(root:&Path)->std::io::Result<Self> {Self::connect(root).map_err(invalid_source)}
    fn connect(root:&Path)->Result<Self> {Ok(Self{rooting:Rooting::new(root),db:database(root,false)?})}
    /// Opens nothing until a first question needs the registry.
    pub(crate) fn deferred(root:&Path)->Self {Self{rooting:Rooting::new(root),db:None}}
    /// A registry created after the pass began is opened once it exists.
    fn db(&mut self)->Result<Option<&Connection>> {
        Ok(self.parts()?.map(|(db,_)|db))
    }
    fn parts(&mut self)->Result<Option<(&Connection,&mut Rooting)>> {
        if self.db.is_none() {self.db=database(&self.rooting.root,false)?;}
        Ok(match self.db.as_ref() {Some(db)=>Some((db,&mut self.rooting)),None=>None})
    }
    /// A standalone source comes before a packed one, and the first library in
    /// order before the others.
    pub(crate) fn frozen(&mut self,hash:&str)->Result<Option<FrozenBodySource>> {
        let Some(db)=self.db()? else {return Ok(None)};
        let row:Option<(i64,String)>=db.query_row("SELECT kind,source FROM (SELECT 0 AS kind,library,source FROM sources WHERE hash=?1
            UNION ALL SELECT 1,library,source FROM packed_sources WHERE hash=?1) ORDER BY kind,library LIMIT 1",[hash],|r|Ok((r.get(0)?,r.get(1)?)))
            .optional().map_err(local)?;
        row.map(|(kind,source)| if kind==0 {serde_json::from_str(&source).map(FrozenBodySource::Standalone)} else {serde_json::from_str(&source).map(FrozenBodySource::Packed)})
            .transpose().map(|source|source.map(|source|source.rooted_by(&mut self.rooting))).map_err(local)
    }
    pub(crate) fn stat(&mut self,hash:&str)->std::io::Result<Option<u64>> {
        self.frozen(hash).map(|source|source.map(|source|source.byte_length())).map_err(invalid_source)
    }
    /// The source `frozen` would choose, for every hash in `hashes` that has one.
    fn registered(&mut self,hashes:&[&str])->Result<BTreeMap<String,Registered>> {
        let mut found=BTreeMap::new();
        let Some((db,rooting))=self.parts()? else {return Ok(found)};
        for page in hashes.chunks(LOOKUP_PAGE) {
            let marks=(1..=page.len()).map(|index|format!("?{index}")).collect::<Vec<_>>().join(",");
            let mut statement=db.prepare(&format!("SELECT hash,0,library,source FROM sources WHERE hash IN ({marks})
                UNION ALL SELECT hash,1,library,source FROM packed_sources WHERE hash IN ({marks}) ORDER BY 1,2,3")).map_err(local)?;
            let mut rows=statement.query(rusqlite::params_from_iter(page)).map_err(local)?;
            while let Some(row)=rows.next().map_err(local)? {
                let hash:String=row.get(0).map_err(local)?;
                if found.contains_key(&hash) {continue;}
                let kind:i64=row.get(1).map_err(local)?;
                let source:String=row.get(3).map_err(local)?;
                let registered=if kind==0 {Registered::Standalone(serde_json::from_str::<Source>(&source).map_err(local)?.rooted(rooting))} else {
                    let outline:OutlineRow=serde_json::from_str(&source).map_err(local)?;
                    let mut packs=outline.chunks.into_iter().map(|chunk|chunk.pack_id).collect::<Vec<_>>();
                    packs.sort();packs.dedup();
                    Registered::Packed((rooting.root(outline.connection_root),outline.connection_id,outline.library_id),
                        PackedOutline{hash:hash.clone(),byte_length:outline.byte_length,packs})
                };
                found.insert(hash,registered);
            }
        }
        Ok(found)
    }
    /// The holders of every hash in `hashes` that has a registered source.
    pub(crate) fn holders(&mut self,hashes:&[&str])->Result<BTreeMap<String,BodyHolders>> {
        let mut found=BTreeMap::<String,BodyHolders>::new();
        let Some((db,rooting))=self.parts()? else {return Ok(found)};
        for page in hashes.chunks(LOOKUP_PAGE) {
            let marks=(1..=page.len()).map(|index|format!("?{index}")).collect::<Vec<_>>().join(",");
            let mut statement=db.prepare(&format!("SELECT hash,0,library,source FROM sources WHERE hash IN ({marks})
                UNION ALL SELECT hash,1,library,source FROM packed_sources WHERE hash IN ({marks}) ORDER BY 1,2,3")).map_err(local)?;
            let mut rows=statement.query(rusqlite::params_from_iter(page)).map_err(local)?;
            while let Some(row)=rows.next().map_err(local)? {
                let hash:String=row.get(0).map_err(local)?;
                let kind:i64=row.get(1).map_err(local)?;
                let source:String=row.get(3).map_err(local)?;
                let (byte_length,connection)=if kind==0 {
                    let source:Source=serde_json::from_str(&source).map_err(local)?;
                    (source.body.plaintext_byte_length.0,(rooting.root(source.connection_root),source.connection_id))
                } else {
                    let outline:OutlineRow=serde_json::from_str(&source).map_err(local)?;
                    (outline.byte_length,(rooting.root(outline.connection_root),outline.connection_id))
                };
                found.entry(hash).or_insert_with(||BodyHolders{byte_length,connections:BTreeSet::new()}).connections.insert(connection);
            }
        }
        Ok(found)
    }
    /// Every hash with a source registered through `connection_id`, in any
    /// library or root.
    pub(crate) fn connection_hashes(&mut self,connection_id:&str)->Result<BTreeSet<String>> {
        let mut found=BTreeSet::new();
        let Some(db)=self.db()? else {return Ok(found)};
        let mut statement=db.prepare("SELECT hash FROM sources WHERE json_extract(source,'$.connectionId')=?1
            UNION SELECT hash FROM packed_sources WHERE json_extract(source,'$.connectionId')=?1").map_err(local)?;
        let mut rows=statement.query([connection_id]).map_err(local)?;
        while let Some(row)=rows.next().map_err(local)? {found.insert(row.get(0).map_err(local)?);}
        Ok(found)
    }
    /// The hashes in `hashes` that have any registered source.
    fn present(&mut self,hashes:&[&str])->Result<BTreeSet<String>> {
        let mut found=BTreeSet::new();
        let Some(db)=self.db()? else {return Ok(found)};
        for page in hashes.chunks(LOOKUP_PAGE) {
            let marks=(1..=page.len()).map(|index|format!("?{index}")).collect::<Vec<_>>().join(",");
            let mut statement=db.prepare(&format!("SELECT hash FROM sources WHERE hash IN ({marks})
                UNION SELECT hash FROM packed_sources WHERE hash IN ({marks})")).map_err(local)?;
            let mut rows=statement.query(rusqlite::params_from_iter(page)).map_err(local)?;
            while let Some(row)=rows.next().map_err(local)? {found.insert(row.get(0).map_err(local)?);}
        }
        Ok(found)
    }
    /// The packed sources `library` registered for `hashes`.
    fn packed<'a>(&mut self,library:&str,hashes:impl IntoIterator<Item=&'a str>)->Result<BTreeMap<String,PackedSource>> {
        let mut found=BTreeMap::new();
        let Some((db,rooting))=self.parts()? else {return Ok(found)};
        let hashes=hashes.into_iter().collect::<Vec<_>>();
        for page in hashes.chunks(LOOKUP_PAGE) {
            let marks=(2..=page.len()+1).map(|index|format!("?{index}")).collect::<Vec<_>>().join(",");
            let mut statement=db.prepare(&format!("SELECT hash,source FROM packed_sources WHERE library=?1 AND hash IN ({marks})")).map_err(local)?;
            let mut rows=statement.query(rusqlite::params_from_iter(std::iter::once(library).chain(page.iter().copied()))).map_err(local)?;
            while let Some(row)=rows.next().map_err(local)? {
                let hash:String=row.get(0).map_err(local)?;
                let source:String=row.get(1).map_err(local)?;
                found.insert(hash,serde_json::from_str::<PackedSource>(&source).map_err(local)?.rooted(rooting));
            }
        }
        Ok(found)
    }
}
fn invalid_source(_:ProviderError)->std::io::Error {std::io::Error::other("external-source-invalid")}
pub(crate) fn register(root: &Path, source: &Source) -> Result<()> {
    risunest_sync_wire::validate_hash(&source.hash).map_err(local)?;
    let locator = source.body.locator.as_ref().ok_or_else(segment::corrupt)?;
    if source.protected_segment.is_empty()
        || locator.object.is_empty()
        || source.connection_id.is_empty()
    {
        return Err(segment::corrupt());
    }
    let db = database(root, true)?.ok_or_else(segment::corrupt)?;
    let previous: Option<String> = db
        .query_row(
            "SELECT source FROM sources WHERE hash=?1 AND library=?2",
            params![source.hash, source.library_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(local)?;
    if let Some(previous) = previous {
        let previous: Source = serde_json::from_str(&previous).map_err(local)?;
        if previous.body.plaintext_byte_length != source.body.plaintext_byte_length {
            return Err(segment::corrupt());
        }
    }
    db.execute("INSERT INTO sources(hash,library,source) VALUES(?1,?2,?3) ON CONFLICT(hash,library) DO UPDATE SET source=excluded.source",
        params![source.hash,source.library_id,serde_json::to_string(source).map_err(local)?]).map_err(local)?;
    Ok(())
}
pub(crate) fn source(root: &Path, hash: &str) -> Result<Option<Source>> {
    let Some(db) = database(root, false)? else {
        return Ok(None);
    };
    let value: Option<String> = db
        .query_row(
            "SELECT source FROM sources WHERE hash=?1 ORDER BY library LIMIT 1",
            [hash],
            |r| r.get(0),
        )
        .optional()
        .map_err(local)?;
    value
        .map(|value| serde_json::from_str::<Source>(&value).map(|source| source.rooted(&mut Rooting::new(root))).map_err(local))
        .transpose()
}
pub(crate) fn stat(root: &Path, hash: &str) -> std::io::Result<Option<u64>> {
    RemoteBodies::open(root)?.stat(hash)
}
/// Forgets every body a removed connection held, so nothing counts or
/// downloads them from it again. `orphaned` receives the bodies no other
/// source holds and runs before the removal commits, so its failure keeps the
/// sources.
pub(crate) fn remove_connection_sources(
    root: &Path,
    connection_id: &str,
    orphaned: impl FnOnce(&[String]) -> std::io::Result<()>,
) -> Result<()> {
    let Some(mut db) = database(root, false)? else {
        return Ok(());
    };
    let transaction = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(local)?;
    let mut hashes = std::collections::BTreeSet::new();
    for table in ["sources", "packed_sources"] {
        let filter = format!("FROM {table} WHERE json_extract(source,'$.connectionId')=?1");
        let mut statement = transaction.prepare(&format!("SELECT hash {filter}")).map_err(local)?;
        for hash in statement.query_map([connection_id], |row| row.get::<_, String>(0)).map_err(local)? {
            hashes.insert(hash.map_err(local)?);
        }
        transaction.execute(&format!("DELETE {filter}"), [connection_id]).map_err(local)?;
    }
    let mut remaining = transaction
        .prepare("SELECT EXISTS(SELECT 1 FROM sources WHERE hash=?1) OR EXISTS(SELECT 1 FROM packed_sources WHERE hash=?1)")
        .map_err(local)?;
    let mut orphans = Vec::new();
    for hash in hashes {
        if !remaining.query_row([&hash], |row| row.get::<_, bool>(0)).map_err(local)? {
            orphans.push(hash);
        }
    }
    drop(remaining);
    orphaned(&orphans).map_err(local)?;
    transaction.commit().map_err(local)
}
pub(crate) fn validate_packed_source<O:Borrow<StoredObject>>(source:&PackedSource<O>,repository:&RepositoryHandle) -> Result<()> {
    risunest_sync_wire::validate_hash(&source.hash).map_err(local)?;
    let catalog:&StoredObject=source.catalog.borrow();
    catalog.validate().map_err(local)?;
    super::packaging::RemoteObject::from_stored(catalog,repository)?;
    if catalog.header.role!=risunest_external_storage_format::snapshot::ObjectRole::Catalog
        || source.protected_snapshot.is_empty() || source.connection_id.is_empty()
        || (source.chunks.is_empty() && source.byte_length!=0) { return Err(segment::corrupt()); }
    let mut length=0u64;
    let packs=source.packs.iter().map(|pack|{let pack:&StoredObject=pack.borrow();(pack.header.object_id.as_str(),pack)}).collect::<BTreeMap<_,_>>();
    if packs.len()!=source.packs.len() {return Err(segment::corrupt());}
    for chunk in &source.chunks {
        let pack=*packs.get(chunk.pack_id.as_str()).ok_or_else(segment::corrupt)?;
        super::packaging::RemoteObject::from_stored(pack,repository)?;
        if pack.header.role!=risunest_external_storage_format::snapshot::ObjectRole::Pack
            || pack.header.repository_id!=catalog.header.repository_id
            || chunk.offset.checked_add(chunk.stored_length).is_none_or(|end|end>pack.plaintext_length) { return Err(segment::corrupt()); }
        length=length.checked_add(chunk.plaintext_length).ok_or_else(segment::corrupt)?;
    }
    if length!=source.byte_length { return Err(segment::corrupt()); }
    Ok(())
}
pub(crate) const PACKED_REGISTRATION_BATCH:usize=1024;
pub(crate) fn register_packed_many<O:Borrow<StoredObject>+Serialize>(root:&Path,sources:&[PackedSource<O>],repository:&RepositoryHandle) -> Result<()> {
    for source in sources {validate_packed_source(source,repository)?;}
    register_verified_packed_many(root,sources)
}
/// Registers the sources through one connection, committing every batch, so a large catalog
/// does not pay a connection and a synchronous commit per body.
pub(super) fn register_verified_packed_many<'a,O:Serialize+'a>(root:&Path,sources:impl IntoIterator<Item=&'a PackedSource<O>>) -> Result<()> {
    let mut sources=sources.into_iter().peekable();
    if sources.peek().is_none() {return Ok(());}
    let mut db=database(root,true)?.ok_or_else(segment::corrupt)?;
    while sources.peek().is_some() {
        let transaction=db.transaction().map_err(local)?;
        for source in sources.by_ref().take(PACKED_REGISTRATION_BATCH) {
            transaction.execute("INSERT INTO packed_sources VALUES(?1,?2,?3) ON CONFLICT(hash,library) DO UPDATE SET source=excluded.source",params![source.hash,source.library_id,serde_json::to_string(source).map_err(local)?]).map_err(local)?;
        }
        transaction.commit().map_err(local)?;
    }
    Ok(())
}
pub(crate) fn packed_source(root:&Path,hash:&str) -> Result<Option<PackedSource>> {
    let Some(db)=database(root,false)? else {return Ok(None)};
    let value:Option<String>=db.query_row("SELECT source FROM packed_sources WHERE hash=?1 ORDER BY library LIMIT 1",[hash],|r|r.get(0)).optional().map_err(local)?;
    value.map(|v|serde_json::from_str::<PackedSource>(&v).map(|source|source.rooted(&mut Rooting::new(root))).map_err(local)).transpose()
}
/// A body grouped with every body it shares a pack with.
trait PackMember {
    fn member_hash(&self)->&str;
    fn pack_ids(&self)->impl Iterator<Item=&str>;
}
impl<O> PackMember for PackedSource<O> {
    fn member_hash(&self)->&str {&self.hash}
    fn pack_ids(&self)->impl Iterator<Item=&str> {self.chunks.iter().map(|chunk|chunk.pack_id.as_str())}
}
impl PackMember for PackedOutline {
    fn member_hash(&self)->&str {&self.hash}
    fn pack_ids(&self)->impl Iterator<Item=&str> {self.packs.iter().map(String::as_str)}
}
pub(crate) fn packed_body_groups<O>(sources:Vec<PackedSource<O>>,priority:&BTreeSet<String>)->Vec<Vec<PackedSource<O>>> {
    pack_groups(sources,priority)
}
fn pack_groups<T:PackMember>(members:Vec<T>,priority:&BTreeSet<String>)->Vec<Vec<T>> {
    let mut parents=(0..members.len()).collect::<Vec<_>>();
    fn root(parents:&mut [usize],mut index:usize)->usize {
        while parents[index]!=index {parents[index]=parents[parents[index]];index=parents[index];}index
    }
    let mut users=BTreeMap::<&str,usize>::new();
    for (index,member) in members.iter().enumerate() {
        for pack in member.pack_ids() {
            if let Some(previous)=users.insert(pack,index) {
                let left=root(&mut parents,index);let right=root(&mut parents,previous);
                parents[left.max(right)]=left.min(right);
            }
        }
    }
    drop(users);
    let mut groups=BTreeMap::<usize,Vec<T>>::new();
    for (index,member) in members.into_iter().enumerate() {groups.entry(root(&mut parents,index)).or_default().push(member);}
    let mut groups=groups.into_values().collect::<Vec<_>>();
    groups.sort_by_key(|group|!group.iter().any(|member|priority.contains(member.member_hash())));
    for group in &mut groups {group.sort_by_key(|member|!priority.contains(member.member_hash()));}
    groups
}
/// Bodies one hold of the repository mutation lock publishes, and the bytes
/// after which a batch closes early. They size a batch, not a hydration: every
/// body is published, in as many batches as it takes.
pub(crate) const PUBLISH_BATCH_BODIES:usize=64;
const _: () = assert!(PUBLISH_BATCH_BODIES <= crate::asset_repository::parallel_stage::MAX_BATCH);
pub(crate) const PUBLISH_BATCH_BYTES:u64=64*1024*1024;
fn hydration_error(_:impl std::fmt::Display)->crate::server_sync::SyncError {crate::server_sync::SyncError::new("external-body-unavailable",502)}
fn remote_error(error:ProviderError)->crate::server_sync::SyncError {
    match error.kind {
        ErrorKind::Cancelled=>crate::server_sync::SyncError::new("cancelled",409),
        // A 401 or 403 asks for the same remedy as an expired sign-in.
        ErrorKind::ReauthRequired|ErrorKind::Unauthorized=>crate::server_sync::SyncError::new("external-reauth-required",502),
        ErrorKind::RepositoryKeyUnavailable|ErrorKind::RecoveryKeyMismatch=>crate::server_sync::SyncError::new("external-key-locked",502),
        _=>crate::server_sync::SyncError::new("external-body-unavailable",502),
    }
}
/// Bodies come from every connection that holds them, so a sign-in or key
/// failure names the connection that has to sign in again or take its key.
fn source_error(connection_id:&str)->impl Fn(ProviderError)->crate::server_sync::SyncError+'_ {
    move |error| {
        let mut value=remote_error(error);
        if ["external-reauth-required","external-key-locked"].contains(&value.code.as_str()) {value.connection_id=Some(connection_id.to_owned());}
        value
    }
}
pub(crate) fn hydrate_registered_many(root:&Path,digests:&[String],priority:&BTreeSet<String>,
    cancellation:Option<Arc<std::sync::atomic::AtomicBool>>,check:&dyn Fn()->crate::server_sync::Result<()>,
    opened:&mut dyn FnMut(&str,crate::server_sync::residency::HydrationOutcome))->crate::server_sync::Result<Vec<String>> {
    hydrate_registered_many_scheduled(root,digests,priority,cancellation,check,opened,&mut |_,_|Ok(()))
}
pub(crate) fn hydrate_registered_many_scheduled(root:&Path,digests:&[String],priority:&BTreeSet<String>,
    cancellation:Option<Arc<std::sync::atomic::AtomicBool>>,check:&dyn Fn()->crate::server_sync::Result<()>,
    opened:&mut dyn FnMut(&str,crate::server_sync::residency::HydrationOutcome),
    before_group:&mut dyn FnMut(&str,&mut dyn FnMut(&str,crate::server_sync::residency::HydrationOutcome))->crate::server_sync::Result<()>) -> crate::server_sync::Result<Vec<String>> {
    use crate::persistent_store::asset_object_catalog::{AssetObjectRegistration,ASSET_OBJECT_CATALOG_MAX_PAGE};
    use crate::server_sync::residency::HydrationOutcome;
    let cancel=cancellation.map(Cancellation::with_external_flag).unwrap_or_default();
    let cas=crate::asset_repository::PayloadCas::new(root)?;
    let scratch=super::leftovers::local_scratch(root,"asset-hydration-")?;
    #[cfg(test)]
    hydration_tests::observe("scratch",root,scratch.path());
    let created_at_ms=i64::try_from(super::runtime::now_ms()).map_err(hydration_error)?;
    let publisher=Publisher{root,cas:&cas,check,cancel:&cancel,catalog:Default::default(),created_at_ms};
    let mut remote=RemoteBodies::connect(root).map_err(hydration_error)?;
    let mut seen=BTreeSet::new();
    let digests=digests.iter().map(String::as_str).filter(|hash|seen.insert(*hash)).collect::<Vec<_>>();
    let mut unavailable=Vec::new();
    let mut standalone=Vec::new();
    let mut packed=BTreeMap::<ConnectionKey,Vec<PackedOutline>>::new();
    for page in digests.chunks(ASSET_OBJECT_CATALOG_MAX_PAGE as usize) {
        publisher.checked()?;
        let mut sizes=BTreeMap::new();
        for (&hash,size) in page.iter().zip(cas.stat_objects(page.iter().copied())?) {
            if let Some(size)=size {sizes.insert(hash,size);}
        }
        let mut registered=remote.registered(page).map_err(hydration_error)?;
        #[cfg(test)]
        hydration_tests::after_lookup(root);
        let mut local=BTreeSet::new();
        {
            // A row goes in before its body's file, so no interruption leaves a published body without one.
            // Until the file arrives, the remote source registered for the body is what the row stands on.
            // Source deletion drops, under this lock, rows left with neither a file nor a source, so each
            // row is checked under it: a body with a source still registered gets its row, and one
            // without a source gets a row only if its file is still there.
            let _guard=crate::asset_repository::coordinator::lock_repository_mutation()?;
            #[cfg(test)]
            let held=std::time::Instant::now();
            let checked=page.iter().copied().filter(|hash|sizes.contains_key(hash)||registered.contains_key(*hash)).collect::<Vec<_>>();
            let present=remote.present(&checked).map_err(hydration_error)?;
            let mut rows=Vec::with_capacity(checked.len());
            for hash in checked {
                let byte_size=if present.contains(hash) {
                    match (sizes.get(hash),registered.get(hash)) {
                        (Some(&size),_)=>{local.insert(hash);size}
                        (None,Some(Registered::Standalone(source)))=>source.body.plaintext_byte_length.0,
                        (None,Some(Registered::Packed(_,outline)))=>outline.byte_length,
                        (None,None)=>continue,
                    }
                } else if let Some(size)=cas.stat_object(hash)? {
                    local.insert(hash);size
                } else {
                    registered.remove(hash);continue;
                };
                rows.push(AssetObjectRegistration{object_hash:hash.to_owned(),byte_size});
            }
            publisher.register(&rows)?;
            #[cfg(test)]
            hydration_tests::record_registration_hold(root,page.len(),held.elapsed());
        }
        for &hash in page {
            if local.contains(hash) {opened(hash,HydrationOutcome::AlreadyLocal);continue;}
            match registered.remove(hash) {
                Some(Registered::Standalone(source))=>standalone.push(source),
                Some(Registered::Packed(connection,outline))=>packed.entry(connection).or_default().push(outline),
                None=>unavailable.push(hash.to_owned()),
            }
        }
        #[cfg(test)]
        hydration_tests::crash_point(root,hydration_tests::CrashPoint::AfterRegistration)?;
    }
    enum Work { Packed(Vec<PackedOutline>), Standalone(Vec<Source>) }
    let positions=digests.iter().enumerate().map(|(index,hash)|(*hash,index)).collect::<BTreeMap<_,_>>();
    let rank=|hash:&str|(!priority.contains(hash),positions.get(hash).copied().unwrap_or(usize::MAX));
    let mut groups=Vec::new();
    for (connection,outlines) in packed {
        for mut group in pack_groups(outlines,priority) {
            group.sort_by_key(|outline|rank(&outline.hash));
            groups.push((rank(&group[0].hash),connection.clone(),Work::Packed(group)));
        }
    }
    for source in standalone {
        let connection=(source.connection_root.clone(),source.connection_id.clone(),source.library_id.clone());
        groups.push((rank(&source.hash),connection,Work::Standalone(vec![source])));
    }
    groups.sort_by_key(|(rank,_,_)|*rank);
    let mut scheduled:Vec<(ConnectionKey,Work)>=Vec::new();
    for (_,connection,work) in groups {
        match (scheduled.last_mut(),work) {
            (Some((previous,Work::Standalone(batch))),Work::Standalone(mut sources)) if *previous==connection && priority.contains(&batch[0].hash)==priority.contains(&sources[0].hash)=>batch.append(&mut sources),
            (_,work)=>scheduled.push((connection,work)),
        }
    }
    let mut connections=BTreeMap::new();
    for (connection,work) in scheduled {
        publisher.checked()?;
        let first=match &work { Work::Standalone(sources)=>&sources[0].hash, Work::Packed(outlines)=>&outlines[0].hash };
        before_group(first,opened)?;
        match work {
            Work::Standalone(sources)=>{
                let connection=source_connection(&mut connections,&connection,&cancel).map_err(source_error(&connection.1))?;
                hydrate_standalone(connection,sources,scratch.path(),&publisher,opened)?;
            }
            Work::Packed(outlines)=>{
                // Read full sources one shared-pack group at a time.
                let mut sources=remote.packed(&connection.2,outlines.iter().map(|outline|outline.hash.as_str())).map_err(hydration_error)?;
                let mut interner=ObjectInterner::default();
                let mut group=Vec::with_capacity(outlines.len());
                for outline in outlines {
                    match sources.remove(&outline.hash) {
                        Some(source) if source.hash==outline.hash && source.connection_root==connection.0 && source.connection_id==connection.1
                            =>group.push(source.interned(&mut interner)),
                        _=>unavailable.push(outline.hash),
                    }
                }
                drop(sources);
                if group.is_empty() {continue;}
                let opened_source=source_connection(&mut connections,&connection,&cancel).map_err(source_error(&connection.1))?;
                hydrate_pack_group(root,opened_source,group,scratch.path(),&publisher,opened)?;
            }
        }
    }
    check()?;Ok(unavailable)
}
/// A source connection, opened once for every group a hydration reads through it.
struct OpenedSource {
    connection:ConnectionKey,
    provider:Arc<dyn Provider>,
    repository:RepositoryHandle,
    key:zeroize::Zeroizing<[u8;32]>,
    stored:StoredConnection,
}
impl OpenedSource {
    fn protection<'a>(&'a self,writer:&'a str)->super::leases::LeaseContext<'a> {
        super::leases::LeaseContext{root:&self.connection.0,connection_id:&self.connection.1,writer_id:writer,descriptor:&self.stored.descriptor,
            root_key:&self.key,provider:self.provider.as_ref(),repository:&self.repository,clock:super::leases::system_clock(),
            protection_supported:self.stored.capabilities.lease_operations,ledger:None}
    }
}
fn source_connection<'a>(connections:&'a mut BTreeMap<ConnectionKey,OpenedSource>,connection:&ConnectionKey,cancel:&Cancellation)->Result<&'a OpenedSource> {
    if !connections.contains_key(connection) {
        let (provider,repository,key,stored)=tauri::async_runtime::block_on(open_source_connection(&connection.0,&connection.1,&connection.2,cancel))?;
        connections.insert(connection.clone(),OpenedSource{connection:connection.clone(),provider,repository,key,stored});
    }
    connections.get(connection).ok_or_else(segment::corrupt)
}
async fn admit(protection:&super::leases::LeaseContext<'_>,cancel:&Cancellation)->Result<super::leases::LeaseOwner> {
    let job=uuid::Uuid::new_v4().to_string();
    match super::leases::admit_shared_work(protection,&job,cancel).await? {
        super::leases::Admission::Admitted(owner)=>Ok(owner),
        super::leases::Admission::Yield{reason}=>Err(super::leases::yield_error(reason)),
        super::leases::Admission::UnsupportedProtection=>Err(ProviderError::new(ErrorKind::Unsupported)),
    }
}
/// Reads every body of one pack group under one lease, then publishes them in batches.
fn hydrate_pack_group(root:&Path,connection:&OpenedSource,group:Vec<SharedPackedSource>,scratch:&Path,publisher:&Publisher,
    opened:&mut dyn FnMut(&str,crate::server_sync::residency::HydrationOutcome))->crate::server_sync::Result<()> {
    use crate::server_sync::residency::HydrationOutcome;
    let OpenedSource{connection:(connection_root,connection_id,library_id),provider,repository,key,stored}=connection;
    let cancel=publisher.cancel;
    let installer_error=std::cell::RefCell::new(None);
    let result=tauri::async_runtime::block_on(async {
        for source in &group {validate_packed_source(source,repository)?;}
        let writer=uuid::Uuid::new_v4().to_string();
        let protection=connection.protection(&writer);
        let owner=admit(&protection,cancel).await?;
        owner.run(&protection,cancel,async {
            let mut missing=Vec::with_capacity(group.len());
            for source in group {
                match publisher.cas.stat_object(&source.hash).map_err(local)? {
                    Some(size) if size!=source.byte_length=>return Err(segment::corrupt()),
                    Some(_)=>opened(&source.hash,HydrationOutcome::AlreadyLocal),
                    None=>missing.push(source),
                }
            }
            if missing.is_empty() {return Ok(());}
            let stage=tempfile::tempdir_in(scratch).map_err(local)?;
            let files=match super::snapshot_restore::download_packed_body_files_scratch(&missing,stage.path(),key,provider.as_ref(),repository,cancel).await {
                Err(error) if error.kind==ErrorKind::NotFound => {
                    let (current_repository,_)=provider.open_repository(&stored.config,&SecretRef(stored.credential_ref.clone()),OpenMode::Existing,cancel).await?;
                    if current_repository.repository_id!=repository.repository_id || current_repository.connection_identity!=repository.connection_identity {return Err(segment::corrupt());}
                    let engine=super::lww_engine::ExternalLwwEngine {provider:provider.clone(),repository:current_repository,library:library_id.clone(),
                        root_key:zeroize::Zeroizing::new(**key),admission:None,connection_id:connection_id.clone(),connection_root:connection_root.clone(),
                        #[cfg(test)]
                        test_lease_clock:None,
                        capabilities:stored.capabilities.clone(),descriptor:stored.descriptor.clone()};
                    let stale=missing.iter().map(SharedPackedSource::unshared).collect::<Vec<_>>();
                    let refreshed=engine.refresh_packed_sources(&stale,&scratch.join("current-catalogs"),cancel).await?;
                    if let Some(reason)=owner.recheck(&protection,cancel).await? {return Err(super::leases::yield_error(reason));}
                    (publisher.check)().map_err(|error| {installer_error.replace(Some(error));segment::corrupt()})?;
                    register_packed_many(root,&refreshed,repository)?;
                    let mut interner=ObjectInterner::default();
                    missing=refreshed.into_iter().map(|source|source.interned(&mut interner)).collect();
                    super::snapshot_restore::download_packed_body_files_scratch(&missing,&stage.path().join("current-source"),key,provider.as_ref(),repository,cancel).await?
                }
                other=>other?,
            };
            // Every read above finished before this check, so one check covers the whole group.
            if let Some(reason)=owner.recheck(&protection,cancel).await? {return Err(super::leases::yield_error(reason));}
            let bodies=missing.iter().map(|source| Ok(StagedBody{hash:source.hash.clone(),size:source.byte_length,
                path:files.get(&source.hash).ok_or_else(segment::corrupt)?.clone()})).collect::<Result<Vec<_>>>()?;
            drop(missing);
            publisher.publish(bodies,opened).map_err(|error| {installer_error.replace(Some(error));segment::corrupt()})
        }).await
    });
    if let Some(error)=installer_error.borrow_mut().take() {return Err(error);}
    result.map_err(source_error(&connection.connection.1))
}
/// Reads the standalone bodies of one connection under one lease, publishing
/// them a batch at a time so the scratch directory holds at most one batch.
fn hydrate_standalone(connection:&OpenedSource,sources:Vec<Source>,scratch:&Path,publisher:&Publisher,
    opened:&mut dyn FnMut(&str,crate::server_sync::residency::HydrationOutcome))->crate::server_sync::Result<()> {
    use crate::server_sync::residency::HydrationOutcome;
    let OpenedSource{provider,repository,key,..}=connection;
    let cancel=publisher.cancel;
    let installer_error=std::cell::RefCell::new(None);
    let result=tauri::async_runtime::block_on(async {
        let writer=uuid::Uuid::new_v4().to_string();
        let protection=connection.protection(&writer);
        let owner=admit(&protection,cancel).await?;
        owner.run(&protection,cancel,async {
            let mut sources=sources.into_iter().peekable();
            while sources.peek().is_some() {
                let mut batch=Vec::new();let mut bytes=0u64;let mut hashes=BTreeSet::new();
                while let Some(source)=sources.next() {
                    cancel.check()?;
                    let size=source.body.plaintext_byte_length.0;
                    if !hashes.insert(source.hash.clone()) {continue;}
                    match publisher.cas.stat_object(&source.hash).map_err(local)? {
                        Some(actual) if actual!=size=>return Err(segment::corrupt()),
                        Some(_)=>opened(&source.hash,HydrationOutcome::AlreadyLocal),
                        None=>{
                            bytes=bytes.saturating_add(size);
                            batch.push(source);
                        }
                    }
                    if batch.len()>=PUBLISH_BATCH_BODIES || bytes>=PUBLISH_BATCH_BYTES {break;}
                }
                let width=provider.transfer_concurrency();
                let stopped=std::sync::atomic::AtomicBool::new(false);
                let mut downloads=futures::stream::iter(batch.iter()).map(|source| {
                    let stopped=&stopped;
                    async move {
                        if stopped.load(std::sync::atomic::Ordering::Acquire) {return (source,Err(ProviderError::new(ErrorKind::Cancelled)));}
                        let result=download_standalone(source,scratch,provider.as_ref(),repository,cancel).await;
                        if result.is_err() {stopped.store(true,std::sync::atomic::Ordering::Release);}
                        (source,result)
                    }
                }).buffered(width);
                let mut spools=Vec::new();let mut failure=None;
                while let Some((source,result))=downloads.next().await {
                    let staged=match result {Ok(staged)=>staged,Err(error)=>{if failure.is_none() {failure=Some(error);}continue;}};
                    let opened=async {
                        cancel.check()?;
                        let mut output=tempfile::NamedTempFile::new_in(scratch).map_err(local)?;
                        let _cpu=super::packaging::cpu_permit().await?;
                        open_standalone(source,&mut output,&staged.path().join("ciphertext"),key)?;
                        Ok(output)
                    }.await;
                    match opened {
                        Ok(output)=>spools.push((source.hash.clone(),source.body.plaintext_byte_length.0,output)),
                        Err(error)=>{stopped.store(true,std::sync::atomic::Ordering::Release);if failure.is_none() {failure=Some(error);}},
                    }
                }
                if !spools.is_empty() {
                    if let Some(reason)=owner.recheck(&protection,cancel).await? {return Err(super::leases::yield_error(reason));}
                    let bodies=spools.iter().map(|(hash,size,file)|StagedBody{hash:hash.clone(),size:*size,path:file.path().to_owned()}).collect();
                    publisher.publish(bodies,opened).map_err(|error| {installer_error.replace(Some(error));segment::corrupt()})?;
                }
                if let Some(error)=failure {return Err(error);}
            }
            Ok(())
        }).await
    });
    if let Some(error)=installer_error.borrow_mut().take() {return Err(error);}
    result.map_err(source_error(&connection.connection.1))
}
/// A verified body waiting in scratch to be staged into the repository.
struct StagedBody {
    hash:String,
    size:u64,
    path:PathBuf,
}
struct Publisher<'a> {
    root:&'a Path,
    cas:&'a crate::asset_repository::PayloadCas,
    check:&'a dyn Fn()->crate::server_sync::Result<()>,
    cancel:&'a Cancellation,
    /// The one catalog connection a hydration registers through, opened when a first row needs it.
    catalog:std::cell::RefCell<Option<crate::persistent_store::asset_object_catalog::AssetObjectRegistrar>>,
    created_at_ms:i64,
}
impl Publisher<'_> {
    fn checked(&self)->crate::server_sync::Result<()> {(self.check)()?;self.cancel.check().map_err(remote_error)}
    fn register(&self,rows:&[crate::persistent_store::asset_object_catalog::AssetObjectRegistration])->crate::server_sync::Result<()> {
        if rows.is_empty() {return Ok(());}
        let mut catalog=self.catalog.borrow_mut();
        if catalog.is_none() {
            #[cfg(test)]
            hydration_tests::observe("catalog",self.root,self.root);
            *catalog=Some(crate::persistent_store::asset_object_catalog::AssetObjectRegistrar::open(self.root).map_err(hydration_error)?);
        }
        match catalog.as_mut() {
            Some(catalog)=>catalog.register(rows,self.created_at_ms).map_err(hydration_error),
            None=>Ok(()),
        }
    }
    fn publish(&self,bodies:Vec<StagedBody>,opened:&mut dyn FnMut(&str,crate::server_sync::residency::HydrationOutcome))->crate::server_sync::Result<()> {
        let mut batch=Vec::new();let mut bytes=0u64;
        for body in bodies {
            bytes=bytes.saturating_add(body.size);batch.push(body);
            if batch.len()>=PUBLISH_BATCH_BODIES || bytes>=PUBLISH_BATCH_BYTES {self.publish_batch(std::mem::take(&mut batch),opened)?;bytes=0;}
        }
        if !batch.is_empty() {self.publish_batch(batch,opened)?;}
        Ok(())
    }
    fn publish_batch(&self,mut batch:Vec<StagedBody>,opened:&mut dyn FnMut(&str,crate::server_sync::residency::HydrationOutcome))->crate::server_sync::Result<()> {
        batch.sort_by(|left,right|left.hash.cmp(&right.hash));
        let mut staged=Vec::with_capacity(batch.len());let mut outcomes=Vec::with_capacity(batch.len());
        self.stage_locked(&batch,&mut staged,&mut outcomes)?;
        for (hash,outcome) in outcomes {opened(hash,outcome);}
        Ok(())
    }
    /// Takes each body's hydration lock in hash order. Preparation and
    /// publication retain every lock, so no other
    /// hydration of these bodies runs between staging and publication.
    fn stage_locked<'b>(&self,rest:&'b [StagedBody],staged:&mut Vec<&'b StagedBody>,
        outcomes:&mut Vec<(&'b str,crate::server_sync::residency::HydrationOutcome)>)->crate::server_sync::Result<()> {
        use crate::server_sync::residency::HydrationOutcome;
        let Some((body,rest))=rest.split_first() else {
            let cas=self.cas;
            let prepared=crate::asset_repository::parallel_stage::stage_batch(staged,&||self.checked(),&|body,stopped| {
                Self::stage(cas,body,&|| {
                    if stopped.load(std::sync::atomic::Ordering::Acquire) {Err(crate::server_sync::SyncError::new("cancelled",409))}
                    else {Ok(())}
                })
            });
            self.checked()?;
            return self.publish_staged(prepared?);
        };
        crate::server_sync::residency::with_hydration_lock(self.root,&body.hash,self.check,|| {
            self.checked()?;
            if let Some(actual)=self.cas.stat_object(&body.hash)? {
                if actual!=body.size {return Err(hydration_error("invalid-body"));}
                outcomes.push((&body.hash,HydrationOutcome::AlreadyLocal));
            } else {
                staged.push(body);
                outcomes.push((&body.hash,HydrationOutcome::Downloaded));
            }
            self.stage_locked(rest,staged,outcomes)
        })
    }
    /// Copies a body into repository staging. The staged copy keeps its sync,
    /// because a torn file of the right size would pass every presence check.
    fn stage(cas:&crate::asset_repository::PayloadCas,body:&StagedBody,check:&dyn Fn()->crate::server_sync::Result<()>)->crate::server_sync::Result<crate::asset_repository::StagedPayload> {
        #[cfg(test)]
        let _scope=crate::asset_repository::body_io::object_scope(&body.hash);
        let input=crate::trust_boundary::open_regular_source(&body.path);
        #[cfg(test)]
        crate::asset_repository::body_io::open_result("managed",&input);
        let input=input?;
        #[cfg(test)]
        let mut input=crate::asset_repository::body_io::TrackedBodyFile::new(input,&body.hash);
        #[cfg(not(test))]
        let mut input=input;
        crate::server_sync::transfer::stage_checked(cas,&mut input,&body.hash,body.size,check)
    }
    fn publish_staged(&self,staged:Vec<crate::asset_repository::StagedPayload>)->crate::server_sync::Result<()> {
        if staged.is_empty() {return Ok(());}
        use crate::asset_repository::job_pins::{CasJobKind,CasJobOwner,DurableCasJob};
        let id=uuid::Uuid::new_v4().to_string();
        let mut journal=DurableCasJob::begin(self.root,&id,CasJobKind::DirectAssetOrInlayWrite,
            CasJobOwner::external_hydration(&id),self.created_at_ms)?;
        #[cfg(test)]
        hydration_tests::before_publish(self.root);
        let guard=crate::asset_repository::coordinator::lock_repository_mutation()?;
        #[cfg(test)]
        let held=std::time::Instant::now();
        self.checked()?;
        #[cfg(test)]
        let count=staged.len();
        journal.record_staged_publication(&staged,&guard)?;
        let published=self.cas.publish_staged_batch(staged)?;
        #[cfg(test)]
        hydration_tests::crash_point(self.root,hydration_tests::CrashPoint::AfterPublish)?;
        // A source deletion while these bodies were read takes the rows of bodies that had no file
        // yet, so each published body is registered again in this hold. A row still there is kept.
        self.register(&published.into_iter().map(|payload|crate::persistent_store::asset_object_catalog::AssetObjectRegistration{
            object_hash:payload.content_hash,byte_size:payload.byte_size}).collect::<Vec<_>>())?;
        #[cfg(test)]
        hydration_tests::crash_point(self.root,hydration_tests::CrashPoint::AfterPublishedRegistration)?;
        journal.finish_catalog_registration(&guard)?;
        #[cfg(test)]
        hydration_tests::record_hold(self.root,count,held.elapsed());
        Ok(())
    }
}
pub(crate) fn hydrate_registered(root:&Path,hash:&str,check:&dyn Fn()->crate::server_sync::Result<()>)->crate::server_sync::Result<bool> {
    Ok(hydrate_registered_many(root,&[hash.to_owned()],&std::collections::BTreeSet::new(),None,check,&mut |_,_| {})?.is_empty())
}
pub(crate) fn hydrate(
    root: &Path,
    hash: &str,
    check: &dyn Fn() -> crate::server_sync::Result<()>,
) -> crate::server_sync::Result<Option<std::fs::File>> {
    let cas = crate::asset_repository::PayloadCas::new(root)?;
    if let Some(file) = cas.open_object(hash)? {
        return Ok(Some(file));
    }
    if !hydrate_registered(root,hash,check)? {return Ok(None);}
    check()?;cas.open_object(hash).map_err(Into::into)
}

pub(crate) async fn spool_verified_remote_body(root:&Path,hash:&str,destination:&Path,cancel:&Cancellation)->Result<Option<tempfile::NamedTempFile>> {
    spool_verified_remote_body_observed(root,hash,destination,cancel,None).await
}
pub(crate) async fn spool_verified_remote_body_observed(root:&Path,hash:&str,destination:&Path,cancel:&Cancellation,observer:Option<std::sync::Arc<dyn Fn(u64)+Send+Sync>>)->Result<Option<tempfile::NamedTempFile>> {
    let Some(source)=freeze_remote_body(root,hash)? else {return Ok(None)};
    spool_frozen_remote_body_observed(&source,destination,cancel,observer).await.map(Some)
}
pub(crate) async fn spool_frozen_remote_body(source:&FrozenBodySource,destination:&Path,cancel:&Cancellation)->Result<tempfile::NamedTempFile> {
    spool_frozen_remote_body_observed(source,destination,cancel,None).await
}
/// The body comes from the connection that registered it, which can be another
/// than the one the operation works on, so a failure that only that connection's
/// sign-in or key clears names it.
pub(super) async fn spool_frozen_remote_body_observed(source:&FrozenBodySource,destination:&Path,cancel:&Cancellation,observer:Option<std::sync::Arc<dyn Fn(u64)+Send+Sync>>)->Result<tempfile::NamedTempFile> {
    let connection_id=match source {FrozenBodySource::Standalone(source)=>&source.connection_id,FrozenBodySource::Packed(source)=>&source.connection_id};
    spool_frozen_remote_body_from(source,destination,cancel,observer).await.map_err(|mut error| {
        if matches!(error.kind,ErrorKind::ReauthRequired|ErrorKind::Unauthorized|ErrorKind::RepositoryKeyUnavailable|ErrorKind::RecoveryKeyMismatch) {
            error.connection_id=Some(connection_id.clone());
        }
        error
    })
}
async fn spool_frozen_remote_body_from(source:&FrozenBodySource,destination:&Path,cancel:&Cancellation,observer:Option<std::sync::Arc<dyn Fn(u64)+Send+Sync>>)->Result<tempfile::NamedTempFile> {
    let high_water=std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let progress=|| {
        let observer=observer.clone(); let high_water=high_water.clone();
        super::phase_progress::PhaseProgress::new(move |reading| {
            let previous=high_water.fetch_max(reading.bytes,std::sync::atomic::Ordering::Relaxed);
            if reading.bytes>previous {if let Some(observer)=&observer {observer(reading.bytes-previous);}}
        })
    };
    let (hash,connection_root,connection_id,library_id)=match source {
        FrozenBodySource::Standalone(source)=>(&source.hash,&source.connection_root,&source.connection_id,&source.library_id),
        FrozenBodySource::Packed(source)=>(&source.hash,&source.connection_root,&source.connection_id,&source.library_id),
    };
    let (provider,repository,key,stored)=open_source_connection(connection_root,connection_id,library_id,cancel).await?;
    if let FrozenBodySource::Packed(source)=source {validate_packed_source(source,&repository)?;}
    risunest_sync_wire::validate_hash(hash).map_err(local)?;
    let connection_root=connection_root.clone();
    let connection_id=connection_id.clone();
    let source=source.clone();
    let writer=uuid::Uuid::new_v4().to_string();
    let protection=super::leases::LeaseContext{root:&connection_root,connection_id:&connection_id,writer_id:&writer,descriptor:&stored.descriptor,root_key:&key,provider:provider.as_ref(),repository:&repository,clock:super::leases::system_clock(),protection_supported:stored.capabilities.lease_operations,ledger:None};
    let owner=admit(&protection,cancel).await?;
    owner.run(&protection,cancel,async {
        std::fs::create_dir_all(destination).map_err(local)?;
        let mut output=tempfile::NamedTempFile::new_in(destination).map_err(local)?;
        if let FrozenBodySource::Standalone(source)=&source {
            let progress=progress();
            let scratch=download_standalone_observed(source,destination,provider.as_ref(),&repository,cancel,Some(&progress)).await?;
            open_standalone(source,&mut output,&scratch.path().join("ciphertext"),&key)?;
        } else if let FrozenBodySource::Packed(source)=&source {
            let scratch=tempfile::tempdir_in(destination).map_err(local)?;
            let path=match super::snapshot_restore::download_packed_body_file_observed(hash,source.byte_length,source.chunks.clone(),source.packs.clone(),scratch.path(),&key,provider.as_ref(),&repository,&progress(),cancel).await {
                Err(error) if error.kind==ErrorKind::NotFound => {
                    let (current_repository,_)=provider.open_repository(&stored.config,&SecretRef(stored.credential_ref.clone()),OpenMode::Existing,cancel).await?;
                    if current_repository.repository_id!=repository.repository_id || current_repository.connection_identity!=repository.connection_identity {return Err(segment::corrupt());}
                    let engine=super::lww_engine::ExternalLwwEngine{provider:provider.clone(),repository:current_repository,library:source.library_id.clone(),
                        root_key:zeroize::Zeroizing::new(*key),admission:None,connection_id:connection_id.clone(),connection_root:connection_root.clone(),
                        #[cfg(test)]
                        test_lease_clock:None,
                        capabilities:stored.capabilities.clone(),descriptor:stored.descriptor.clone()};
                    let mut refreshed=engine.refresh_packed_sources(std::slice::from_ref(source),&scratch.path().join("current-catalogs"),cancel).await?;
                    let refreshed=refreshed.pop().ok_or_else(segment::corrupt)?;
                    if let Some(reason)=owner.recheck(&protection,cancel).await? {return Err(super::leases::yield_error(reason));}
                    super::snapshot_restore::download_packed_body_file_observed(hash,source.byte_length,refreshed.chunks,refreshed.packs,&scratch.path().join("current-source"),&key,provider.as_ref(),&repository,&progress(),cancel).await?
                }
                other=>other?,
            };
            let mut input=crate::trust_boundary::open_regular_source(&path).map_err(local)?;
            std::io::copy(&mut input,&mut output).map_err(local)?;
        }
        // The spool is read back by this process and never reopened, so it is not synced.
        std::io::Seek::seek(output.as_file_mut(),std::io::SeekFrom::Start(0)).map_err(local)?;
        if let Some(reason)=owner.recheck(&protection,cancel).await? {return Err(super::leases::yield_error(reason));}
        Ok(output)
    }).await
}
async fn download_standalone(source:&Source,destination:&Path,provider:&dyn Provider,
    repository:&RepositoryHandle,cancel:&Cancellation)->Result<tempfile::TempDir> {
    download_standalone_observed(source,destination,provider,repository,cancel,None).await
}
async fn download_standalone_observed(source:&Source,destination:&Path,provider:&dyn Provider,
    repository:&RepositoryHandle,cancel:&Cancellation,progress:Option<&super::phase_progress::PhaseProgress>)->Result<tempfile::TempDir> {
    cancel.check()?;
    let scratch=tempfile::tempdir_in(destination).map_err(local)?;
    let locator=source.body.locator.as_ref().ok_or_else(segment::corrupt)?;
    locator.validate_for(repository)?;
    let cipher=scratch.path().join("ciphertext");
    let mut sink=super::transfer::SpoolSink::create(&cipher,source.body.byte_length.0)?;
    let streamed=progress.map(|phase| {
        phase.plan(1,source.body.byte_length.0);
        let streamed=super::phase_progress::DownloadProgress::new(phase);
        let observed=streamed.clone();
        sink.observe(std::sync::Arc::new(move |bytes|observed.advance(bytes)));
        streamed
    });
    let receipt=match provider.read_object(repository,locator,None,&mut sink,cancel).await? {
        ReadReceipt::Body(receipt)=>receipt,
        ReadReceipt::NotModified(_)=>return Err(segment::corrupt()),
    };
    if receipt.locator!=*locator || !receipt.complete || receipt.byte_length!=source.body.byte_length.0 || !sink.is_verified() {return Err(segment::corrupt());}
    let _verified=super::transfer::SpoolSource::verified(&cipher,source.body.byte_length.0,&source.body.sha256)?;
    if let Some(streamed)=streamed {streamed.commit(source.body.byte_length.0);}
    if let Some(progress)=progress {progress.flush();}
    Ok(scratch)
}
fn open_standalone(source:&Source,output:&mut tempfile::NamedTempFile,cipher:&Path,key:&[u8;32])->Result<()> {
    let mut input=crate::trust_boundary::open_regular_source(cipher).map_err(local)?;
    segment::open_body_stream(&mut input,output,&source.library_id,&source.body.object_id,key,source.body.plaintext_byte_length.0)?;
    if !super::snapshot_restore::verify_body_file(output.path(),source.body.plaintext_byte_length.0,&source.hash)? {return Err(segment::corrupt());}
    Ok(())
}
#[cfg(test)]
mod registration_tests {
    use super::*;
    use risunest_external_storage_format::snapshot::{ObjectRole,PublicObjectHeader,StoredObject,WireLocator};
    #[test]
    fn batched_registration_records_every_source_across_batches() {
        let root=tempfile::tempdir().unwrap();
        register_verified_packed_many(root.path(),&[] as &[PackedSource]).unwrap();
        assert!(database(root.path(),false).unwrap().is_none(),"nothing to register creates no database");
        let catalog=StoredObject {
            header:PublicObjectHeader::new("synthetic-repository".into(),"synthetic-catalog".into(),ObjectRole::Catalog,1).unwrap(),
            locator:WireLocator {connection_identity:"synthetic-account".into(),collection:Some("catalogs".into()),object:"synthetic-object".into()},
            ciphertext_length:1,ciphertext_sha256:[1;32],plaintext_length:1,plaintext_sha256:[2;32],
        };
        let sources=(0..PACKED_REGISTRATION_BATCH*2+1).map(|index| PackedSource {
            hash:format!("{index:064x}"),byte_length:index as u64,library_id:"synthetic-library".into(),
            connection_id:"synthetic-connection".into(),connection_root:PathBuf::new(),protected_snapshot:"synthetic-snapshot".into(),
            catalog:catalog.clone(),chunks:Vec::new(),packs:Vec::new(),
        }).collect::<Vec<_>>();
        register_verified_packed_many(root.path(),&sources).unwrap();
        let db=database(root.path(),false).unwrap().unwrap();
        let count:i64=db.query_row("SELECT COUNT(*) FROM packed_sources",[],|row|row.get(0)).unwrap();
        assert_eq!(count,sources.len() as i64);
        for source in [&sources[0],&sources[PACKED_REGISTRATION_BATCH],&sources[sources.len()-1]] {
            assert_eq!(packed_source(root.path(),&source.hash).unwrap().unwrap().byte_length,source.byte_length);
        }
    }

    #[test]
    fn a_stored_root_that_no_longer_names_the_data_root_without_a_link_reads_as_the_data_root() {
        let fixture=tempfile::tempdir().unwrap();
        let root=fixture.path().join("data");
        std::fs::create_dir(&root).unwrap();
        // A data root moved through a link leaves rows naming the link.
        let link=fixture.path().join("earlier");
        crate::app_paths::tests::link_directory(&root,&link);
        let canonical=std::fs::canonicalize(&root).unwrap();
        let catalog=StoredObject {
            header:PublicObjectHeader::new("synthetic-repository".into(),"synthetic-catalog".into(),ObjectRole::Catalog,1).unwrap(),
            locator:WireLocator {connection_identity:"synthetic-account".into(),collection:Some("catalogs".into()),object:"synthetic-object".into()},
            ciphertext_length:1,ciphertext_sha256:[1;32],plaintext_length:1,plaintext_sha256:[2;32],
        };
        let stored=[(fixture.path().join("gone"),root.clone()),(link,root.clone()),(canonical.clone(),canonical)];
        let sources=stored.iter().enumerate().map(|(index,(connection_root,_))| PackedSource {
            hash:format!("{index:064x}"),byte_length:index as u64,library_id:"synthetic-library".into(),
            connection_id:"synthetic-connection".into(),connection_root:connection_root.clone(),protected_snapshot:"synthetic-snapshot".into(),
            catalog:catalog.clone(),chunks:Vec::new(),packs:Vec::new(),
        }).collect::<Vec<_>>();
        register_verified_packed_many(&root,&sources).unwrap();
        let mut remote=RemoteBodies::open(&root).unwrap();
        for (source,(_,expected)) in sources.iter().zip(&stored) {
            let hash=source.hash.as_str();
            assert_eq!(&packed_source(&root,hash).unwrap().unwrap().connection_root,expected);
            let Some(FrozenBodySource::Packed(frozen))=remote.frozen(hash).unwrap() else {panic!("packed source")};
            assert_eq!(&frozen.connection_root,expected);
            let Some(Registered::Packed(connection,_))=remote.registered(&[hash]).unwrap().remove(hash) else {panic!("packed outline")};
            assert_eq!(&connection.0,expected);
            assert_eq!(&remote.packed("synthetic-library",[hash]).unwrap()[hash].connection_root,expected);
            let holders=remote.holders(&[hash]).unwrap().remove(hash).unwrap();
            assert_eq!(holders.connections,BTreeSet::from([(expected.clone(),"synthetic-connection".to_owned())]));
        }
    }
}
#[cfg(test)]
#[path="lww_residency_hydration_tests.rs"]
mod hydration_tests;
#[cfg(test)]
#[path="lww_download_tests.rs"]
mod download_tests;
#[cfg(test)]
pub(crate) use hydration_tests::{forget_registry_opens, register_synthetic_source, registry_opens, synthetic_packed};
#[cfg(test)]
fn test_source_connections()->&'static std::sync::Mutex<std::collections::BTreeMap<(std::path::PathBuf,String),std::sync::Arc<super::connection_commands::ConnectedRepository>>> {
    static CONNECTIONS:std::sync::OnceLock<std::sync::Mutex<std::collections::BTreeMap<(std::path::PathBuf,String),std::sync::Arc<super::connection_commands::ConnectedRepository>>>>=std::sync::OnceLock::new();
    CONNECTIONS.get_or_init(Default::default)
}
#[cfg(test)]
pub(crate) struct TestSourceConnection { key:(std::path::PathBuf,String) }
#[cfg(test)]
impl Drop for TestSourceConnection {
    fn drop(&mut self) {if let Ok(mut connections)=test_source_connections().lock() {connections.remove(&self.key);}}
}
#[cfg(test)]
pub(crate) fn install_test_source_connection(root:&Path,connection:std::sync::Arc<super::connection_commands::ConnectedRepository>)->Result<TestSourceConnection> {
    let key=(root.to_owned(),connection.stored.id.clone());
    let mut connections=test_source_connections().lock().map_err(super::runtime::local_error)?;
    if connections.contains_key(&key) {return Err(ProviderError::new(ErrorKind::PreconditionFailed));}
    connections.insert(key.clone(),connection);
    Ok(TestSourceConnection{key})
}
/// Opens a body source's connection, with the stored record it was opened from.
pub(crate) async fn open_source_connection(connection_root:&Path,connection_id:&str,library_id:&str,cancel:&Cancellation)->Result<(std::sync::Arc<dyn Provider>,RepositoryHandle,zeroize::Zeroizing<[u8;32]>,StoredConnection)> {
    #[cfg(test)]
    hydration_tests::observe("source-connection",connection_root,connection_root);
    #[cfg(test)]
    {
        let connection=test_source_connections().lock().map_err(super::runtime::local_error)?.get(&(connection_root.to_owned(),connection_id.to_owned())).cloned();
        if let Some(connection)=connection {
            cancel.check()?;
            if connection.stored.descriptor.repository_id!=library_id {return Err(segment::corrupt());}
            let (repository,_)=connection.provider.open_repository(&connection.stored.config,&SecretRef(connection.stored.credential_ref.clone()),OpenMode::Existing,cancel).await?;
            if repository.repository_id!=connection.handle.repository_id || repository.connection_identity!=connection.handle.connection_identity {return Err(segment::corrupt());}
            return Ok((connection.provider.clone(),repository,zeroize::Zeroizing::new(*connection.root_key),connection.stored.clone()));
        }
    }
    let stored = ConnectionStore::open(connection_root)?.read(connection_id)?;
    if stored.descriptor.repository_id != library_id {
        return Err(segment::corrupt());
    }
    let load = || {
        let dependencies = super::connection::dependencies_for_config(connection_root, &stored.config)?;
        super::connection::provider_for(&stored.config, dependencies)
    };
    #[cfg(test)]
    let injected = super::transfer_factory_tests::inputs(connection_root, connection_id);
    #[cfg(test)]
    let provider = match &injected { Some(inputs) => inputs.provider.clone(), None => load()? };
    #[cfg(not(test))]
    let provider = load()?;
    let provider = super::transfer_limit::wrap(connection_root,connection_id,provider)?;
    let (repository, _) = provider
        .open_repository(
            &stored.config,
            &SecretRef(stored.credential_ref.clone()),
            OpenMode::Existing,
            cancel,
        )
        .await?;
    let vault = super::secrets::repository_key_vault(connection_root);
    let load_key = super::connection_commands::read_root_key(
        vault.as_ref(),
        &stored.root_key_ref,
    );
    #[cfg(test)]
    let key = match injected { Some(inputs) => { drop(load_key); inputs.root_key }, None => load_key.await? };
    #[cfg(not(test))]
    let key = load_key.await?;
    Ok((provider, repository, key, stored))
}
