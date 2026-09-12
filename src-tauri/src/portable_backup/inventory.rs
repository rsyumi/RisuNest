//! Physical inventory is separate from alias interpretation. Every permitted source file is
//! retained even if its CAS name or a referring SQL row is damaged.
use super::*;
use crate::{
    asset_repository::{
        job_pins::{CasObjectRole, DurableCasJob},
        PayloadCas,
    },
    persistent_store::PersistentStore,
    trust_boundary::is_link_like,
};
use rusqlite::{params, OptionalExtension};
use std::{fs, path::Path};

const ROOTS: &[&str] = &["assets-v2/objects", "assets", "blobstore", "coldstorage"];

impl Catalog {
    /// Caller holds the exclusive native file admission and a stable revision lease. Registered
    /// files are also enumerated when no live message refers to them. Operational directories
    /// (jobs, sync, recovery and caches) are deliberately outside this inventory.
    pub(crate) fn capture_files(
        &self,
        store: &PersistentStore,
        cas: &PayloadCas,
        pins: &mut DurableCasJob,
        preserving: bool,
        probe: &dyn CancellationProbe,
    ) -> Result<()> {
        self.db.execute_batch("CREATE TEMP TABLE physical_inventory (path TEXT PRIMARY KEY, object_hash BLOB NOT NULL, byte_length INTEGER NOT NULL)")?;
        self.capture_preserved_sources(store.repository_root(), probe)?;
        for root in ROOTS {
            let mut path = store.repository_root().to_path_buf();
            let mut absent = false;
            for component in root.split('/') {
                path.push(component);
                match fs::symlink_metadata(&path) {
                    Ok(metadata) if is_link_like(&metadata) => {
                        return Err(Error::Invalid("linked backup source directory"))
                    }
                    Ok(metadata) if !metadata.is_dir() => {
                        return Err(Error::Invalid("backup source root is not a directory"))
                    }
                    Ok(_) => (),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        absent = true;
                        break;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            if !absent {
                self.capture_directory(
                    store.repository_root(),
                    &path,
                    cas,
                    pins,
                    preserving,
                    probe,
                )?;
            }
        }
        // Bind aliases only after the complete physical inventory has been captured. This avoids
        // dropping registered missing objects or reading the same mutable file for each alias.
        if self.db.query_row(
            "SELECT value='source-sqlite' FROM backup_info WHERE key='profile'",
            [],
            |r| r.get::<_, bool>(0),
        )? {
            self.db.execute_batch("DROP TABLE physical_inventory")?;
            return Ok(());
        }
        for (sql,kind) in [
            ("SELECT logical_key,object_hash,size FROM asset_aliases WHERE kind='asset'","asset"),
            ("SELECT logical_key,object_hash,size FROM asset_aliases WHERE kind='inlay'","inlay"),
            ("SELECT key,object_hash,size FROM cold_aliases","cold"),
            ("SELECT owner_kind||':'||owner_locator,manifest_hash,NULL FROM asset_owner_heads WHERE present=1","owner"),
        ] {
            let mut statement=self.db.prepare(sql)?;let mut rows=statement.query([])?;
            while let Some(row)=rows.next()? {
                check(probe)?;
                let (Ok(key),Ok(hash),Ok(size))=(row.get::<_,String>(0),row.get::<_,Option<String>>(1),row.get::<_,Option<i64>>(2)) else {
                    self.diagnostic("invalid-file-row",kind)?;continue;
                };
                if key.is_empty()||key.contains('\0') {self.diagnostic("invalid-file-key",kind)?;continue;}
                let Some(hash)=hash.filter(|v|hash_valid(v)) else {
                    self.add_file(kind,&key,"{}",None,None,probe)?;continue;
                };
                let path=format!("assets-v2/objects/{}/{}",&hash[..2],&hash[2..]);
                let physical:Option<(Vec<u8>,i64)>=self.db.query_row("SELECT object_hash,byte_length FROM physical_inventory WHERE path=?1",[&path],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
                match physical {
                    Some((actual,bytes))=>{
                        let damaged=hex::encode(&actual)!=hash||size.is_some_and(|n|n!=bytes);
                        if damaged&&!preserving {return Err(Error::Invalid("registered source payload differs"));}
                        self.db.execute("INSERT INTO files VALUES(?1,?2,?3,?4,'{}',?5)",params![kind,key,actual,hash,if damaged {"damaged"}else{"present"}])?;
                        if damaged {self.diagnostic("damaged-registered-file",&key)?;}
                    },
                    None=>self.add_file(kind,&key,"{}",None,Some(&hash),probe)?,
                }
            }
        }
        // Durable registered-but-unreferenced objects are part of the source too. A missing
        // registered file remains an explicit record rather than disappearing during enumeration.
        let mut cursor = None;
        loop {
            check(probe)?;
            let page = store.query_asset_object_catalog(4096, cursor.as_deref())?;
            for object in page.items {
                let path = format!(
                    "assets-v2/objects/{}/{}",
                    &object.object_hash[..2],
                    &object.object_hash[2..]
                );
                let found: bool = self.db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM physical_inventory WHERE path=?1)",
                    [&path],
                    |r| r.get(0),
                )?;
                if !found {
                    self.add_file(
                        "preserved",
                        &path,
                        "{\"registered\":true}",
                        None,
                        Some(&object.object_hash),
                        probe,
                    )?;
                }
            }
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        self.db.execute_batch("DROP TABLE physical_inventory")?;
        Ok(())
    }

    fn diagnostic(&self, code: &str, subject: &str) -> Result<()> {
        self.db.execute(
            "INSERT INTO diagnostics VALUES(?1,?2)",
            params![code, subject],
        )?;
        Ok(())
    }

    fn capture_directory(
        &self,
        root: &Path,
        path: &Path,
        cas: &PayloadCas,
        pins: &mut DurableCasJob,
        preserving: bool,
        probe: &dyn CancellationProbe,
    ) -> Result<()> {
        for entry in fs::read_dir(path)? {
            check(probe)?;
            let entry = entry?;
            let path = entry.path();
            let before = fs::symlink_metadata(&path)?;
            if is_link_like(&before) {
                return Err(Error::Invalid("linked backup source file"));
            }
            if before.is_dir() {
                self.capture_directory(root, &path, cas, pins, preserving, probe)?;
                continue;
            }
            if !before.is_file() {
                return Err(Error::Invalid("nonregular backup source file"));
            }
            let relative = path
                .strip_prefix(root)
                .map_err(|_| Error::Invalid("backup source outside repository"))?
                .to_str()
                .ok_or(Error::Invalid("non-Unicode source file name"))?
                .replace('\\', "/");
            let cas_hash = relative
                .strip_prefix("assets-v2/objects/")
                .and_then(|tail| {
                    let (first, last) = tail.split_once('/')?;
                    let hash = format!("{first}{last}");
                    (first.len() == 2 && last.len() == 62 && hash_valid(&hash)).then_some(hash)
                });
            if let Some(expected) = cas_hash {
                // Pin before opening immutable CAS bytes. Pin role must match all uses of the
                // same object, including owner manifests which are also unreferenced files.
                let owner: bool = self.db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM asset_owner_heads WHERE manifest_hash=?1)",
                    [&expected],
                    |r| r.get(0),
                )?;
                pins.pin_existing(
                    cas,
                    &expected,
                    before.len(),
                    if owner {
                        CasObjectRole::OwnerManifest
                    } else {
                        CasObjectRole::DirectObject
                    },
                )?;
                let mut input = fs::File::open(&path)?;
                let actual = copy_hash(&mut input, &mut io::sink(), before.len(), probe)?;
                if actual != expected && !preserving {
                    return Err(Error::Invalid("registered source payload differs"));
                }
                let mut extra = [0];
                if input.read(&mut extra)? != 0 {
                    return Err(Error::Invalid("source grew during inventory"));
                }
                self.add_pinned_file(
                    "preserved",
                    &relative,
                    "{\"storage\":\"cas\"}",
                    &path,
                    before.len(),
                    &actual,
                    probe,
                )?;
                if actual != expected {
                    self.diagnostic("damaged-cas-file", &relative)?;
                }
            } else {
                self.add_file(
                    "preserved",
                    &relative,
                    "{\"storage\":\"unclassified\"}",
                    Some((&path, before.len())),
                    None,
                    probe,
                )?;
            }
            let after = fs::symlink_metadata(&path)?;
            if is_link_like(&after)
                || !after.is_file()
                || before.len() != after.len()
                || before.modified()? != after.modified()?
                || before.created().ok() != after.created().ok()
            {
                return Err(Error::Invalid("source changed during inventory"));
            }
            self.db.execute("INSERT INTO physical_inventory SELECT logical_key,object_hash,?2 FROM files WHERE kind='preserved' AND logical_key=?1",params![relative,i64::try_from(before.len()).map_err(|_|Error::Invalid("source file too large"))?])?;
        }
        Ok(())
    }
}
