use super::{
    cache::Cache,
    client::{response_error, ServerClient},
    Result, SyncError,
};
use reqwest::Method;
use risunest_sync_wire::{
    canonical, delta, hash,
    transfer::{self, Frame},
    Sequence, MAX_METADATA_BYTES,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    io::{Read, Seek, SeekFrom},
};
const CHUNK: usize = 8 * 1024 * 1024;

/// Resume metadata contains only content identities and server staging IDs.
/// Every resumed chunk lives in the verified cache CAS before its row is saved.
pub(crate) struct Transfer<'a> {
    pub client: &'a ServerClient,
    pub cache: &'a Cache,
    db: Connection,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UploadProgress {
    upload_id: String,
    manifest: Manifest,
    chunk_bytes: Sequence,
    verified: Vec<Sequence>,
    next_after: Option<Sequence>,
    complete: bool,
    finishing: bool,
    failure: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    hash: String,
    size: Sequence,
}
impl<'a> Transfer<'a> {
    pub fn download_record(
        &self,
        version: &risunest_sync_wire::RecordVersion,
        bases: &[String],
    ) -> Result<Vec<String>> {
        use risunest_sync_wire::descriptor::{
            RecordDescriptor, ReferencePage, MAX_DESCRIPTOR_REFERENCES, MAX_TREE_DEPTH,
        };
        let risunest_sync_wire::RecordVersion::Live {
            object_hash,
            descriptor_hash: Some(descriptor_hash),
        } = version
        else {
            return Err(SyncError::new("server-descriptor-required", 409));
        };
        self.download(&[object_hash.clone(), descriptor_hash.clone()], bases)?;
        let descriptor: RecordDescriptor = canonical::decode(
            &self.cache.read(descriptor_hash, MAX_METADATA_BYTES)?,
            MAX_METADATA_BYTES,
        )?;
        descriptor.validate()?;
        if descriptor.object_hash != *object_hash {
            return Err(SyncError::new("descriptor-record-mismatch", 409));
        }
        let mut pending = descriptor
            .dependency_root
            .into_iter()
            .chain(descriptor.relation_root)
            .map(|h| (h, 0usize))
            .collect::<Vec<_>>();
        let mut seen = BTreeSet::new();
        let mut dependencies = BTreeSet::new();
        while !pending.is_empty() {
            let current = std::mem::take(&mut pending);
            self.download(
                &current.iter().map(|(h, _)| h.clone()).collect::<Vec<_>>(),
                bases,
            )?;
            for (hash, depth) in current {
                if !seen.insert(hash.clone())
                    || depth >= MAX_TREE_DEPTH
                    || seen.len() > MAX_DESCRIPTOR_REFERENCES
                {
                    return Err(SyncError::new("invalid-descriptor-tree", 409));
                }
                let page: ReferencePage = canonical::decode(
                    &self.cache.read(&hash, MAX_METADATA_BYTES)?,
                    MAX_METADATA_BYTES,
                )?;
                page.validate()?;
                match page {
                    ReferencePage::Branches { children } => {
                        pending.extend(children.into_iter().map(|h| (h, depth + 1)))
                    }
                    ReferencePage::Objects { hashes } => dependencies.extend(hashes),
                    ReferencePage::Relations { .. } => (),
                }
                if dependencies.len() > MAX_DESCRIPTOR_REFERENCES {
                    return Err(SyncError::new("invalid-descriptor-tree", 409));
                }
            }
        }
        self.download(&dependencies.into_iter().collect::<Vec<_>>(), bases)?;
        self.cache.closure(version)
    }
    pub fn new(client: &'a ServerClient, cache: &'a Cache) -> Result<Self> {
        let db = Connection::open(cache.cas.repository_root().join("transfers.sqlite"))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS uploads(hash TEXT PRIMARY KEY,id TEXT NOT NULL,size TEXT NOT NULL); CREATE TABLE IF NOT EXISTS chunks(target TEXT NOT NULL,part INTEGER NOT NULL,hash TEXT NOT NULL,size INTEGER NOT NULL,PRIMARY KEY(target,part));")?;
        Ok(Self { client, cache, db })
    }
    pub fn upload(&self, hashes: &[String], base_candidates: &[String]) -> Result<()> {
        for page in hashes.chunks(1024) {
            let mut descriptors = Vec::with_capacity(page.len());
            for hash in page {
                let size = self
                    .cache
                    .cas
                    .stat_object(hash)?
                    .ok_or_else(|| SyncError::new("cached-object-missing", 409))?;
                descriptors.push(serde_json::json!({"hash":hash,"size":size.to_string()}));
            }
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Missing {
                missing: Vec<String>,
            }
            let (_, missing): (_, Missing) = self.client.json(
                Method::POST,
                "objects/missing",
                &[],
                Some(&descriptors),
                &[],
            )?;
            let absent: BTreeSet<&str> = missing.missing.iter().map(String::as_str).collect();
            if absent.len() != missing.missing.len()
                || absent.iter().any(|h| !page.iter().any(|p| p == h))
            {
                return Err(SyncError::new("invalid-missing-response", 502));
            }
            let present = page
                .iter()
                .filter(|h| !absent.contains(h.as_str()))
                .cloned()
                .collect::<Vec<_>>();
            let candidates = base_candidates
                .iter()
                .filter(|h| !present.contains(h))
                .cloned()
                .collect::<Vec<_>>();
            let mut leased = present.clone();
            leased.extend(candidates);
            let bases_pinned = match self.pin(&leased) {
                Ok(()) => true,
                Err(e) if e.status == 404 => {
                    self.pin(&present)?;
                    false
                }
                Err(e) => return Err(e),
            };
            let mut frames = Vec::new();
            let mut used = 8usize;
            let mut materialized = 0usize;
            for target in &missing.missing {
                let size = self
                    .cache
                    .cas
                    .stat_object(target)?
                    .ok_or_else(|| SyncError::new("cached-object-missing", 409))?;
                if size > delta::MAX_TARGET_BYTES as u64 {
                    self.upload_large(target, size)?;
                    continue;
                }
                let bytes = self.cache.read(target, delta::MAX_TARGET_BYTES)?;
                let candidates = self.select_bases(target, base_candidates)?;
                let mut frame = None;
                if !candidates.is_empty() {
                    // Missing/collected bases are an explicit full fallback, never
                    // a reason to apply a recipe against a different object.
                    match if bases_pinned {
                        Ok(())
                    } else {
                        self.pin(&candidates)
                    } {
                        Ok(()) => {
                            let bases = candidates
                                .iter()
                                .map(|h| self.cache.read(h, delta::MAX_TARGET_BYTES))
                                .collect::<Result<Vec<_>>>()?;
                            if let Ok(recipe) = delta::create(
                                &bases.iter().map(Vec::as_slice).collect::<Vec<_>>(),
                                &bytes,
                            ) {
                                if recipe.encode()?.len() + 64 < bytes.len() {
                                    frame = Some(Frame::Delta(recipe));
                                }
                            }
                        }
                        Err(e) if e.status == 404 => (),
                        Err(e) => return Err(e),
                    }
                }
                let frame = frame.unwrap_or(Frame::Full(bytes));
                let encoded = transfer::encode(std::slice::from_ref(&frame));
                let length = match encoded {
                    Ok(bytes) => bytes.len() - 8,
                    Err(_) if size >= CHUNK as u64 => {
                        self.upload_large(target, size)?;
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                };
                if used + length > CHUNK || materialized + size as usize > 32 * 1024 * 1024 {
                    self.send_frames(&frames)?;
                    frames.clear();
                    used = 8;
                    materialized = 0;
                }
                used += length;
                materialized += size as usize;
                frames.push(frame);
            }
            self.send_frames(&frames)?;
        }
        Ok(())
    }
    fn send_frames(&self, frames: &[Frame]) -> Result<()> {
        if frames.is_empty() {
            return Ok(());
        }
        let reply = self.client.request(
            Method::POST,
            "uploads/frames",
            &[],
            Some(transfer::encode(frames)?),
            &[],
            MAX_METADATA_BYTES,
        )?;
        if !(200..300).contains(&reply.status) {
            return Err(response_error(reply));
        }
        Ok(())
    }
    pub fn pin(&self, hashes: &[String]) -> Result<()> {
        for page in hashes.chunks(1024) {
            let reply = self.client.request(
                Method::POST,
                "objects/pins",
                &[],
                Some(canonical::encode(&page)?),
                &[],
                MAX_METADATA_BYTES,
            )?;
            if reply.status != 204 {
                return Err(response_error(reply));
            }
        }
        Ok(())
    }
    fn select_bases(&self, target: &str, candidates: &[String]) -> Result<Vec<String>> {
        let mut result = Vec::new();
        let mut total = 0;
        let target_size = self.cache.cas.stat_object(target)?;
        let mut ranked = Vec::new();
        for candidate in candidates {
            if let Some(size) = self.cache.cas.stat_object(candidate)? {
                if size <= delta::MAX_TARGET_BYTES as u64 {
                    ranked.push((candidate, size));
                }
            }
        }
        ranked.sort_by_key(|(_, size)| {
            target_size
                .map(|target| target.abs_diff(*size))
                .unwrap_or(u64::MAX - *size)
        });
        for (candidate, _) in ranked {
            if candidate == target || result.contains(candidate) {
                continue;
            }
            if let Some(size) = self.cache.cas.stat_object(candidate)? {
                if size <= delta::MAX_TARGET_BYTES as u64
                    && total + size <= delta::MAX_BASE_BYTES as u64
                {
                    result.push(candidate.clone());
                    total += size;
                    if result.len() == delta::MAX_BASES {
                        break;
                    }
                }
            }
        }
        Ok(result)
    }
    pub fn download(&self, hashes: &[String], base_candidates: &[String]) -> Result<()> {
        for page in hashes.chunks(1024) {
            let missing = page
                .iter()
                .filter_map(|h| match self.cache.cas.stat_object(h) {
                    Ok(Some(_)) => None,
                    Ok(None) => Some(Ok(h.clone())),
                    Err(e) => Some(Err(SyncError::from(e))),
                })
                .collect::<Result<Vec<_>>>()?;
            if missing.is_empty() {
                continue;
            }
            let mut requests = Vec::new();
            for target in &missing {
                requests.push(serde_json::json!({"target":target,"bases":self.select_bases(target,base_candidates)?}));
            }
            let reply = self.client.request(
                Method::POST,
                "objects/transfer",
                &[],
                Some(canonical::encode(&requests)?),
                &[],
                CHUNK,
            )?;
            if reply.status != 200 {
                return Err(response_error(reply));
            }
            let frames = transfer::decode(&reply.body)?;
            if frames.len() != missing.len() {
                return Err(SyncError::new("transfer-count-mismatch", 502));
            }
            for (target, frame) in missing.iter().zip(frames) {
                match frame {
                    Frame::Full(bytes) => {
                        if hash(&bytes) != *target {
                            return Err(SyncError::new("transfer-target-mismatch", 502));
                        }
                        self.cache.put(&bytes)?;
                    }
                    Frame::Delta(recipe) => {
                        if recipe.target_hash != *target {
                            return Err(SyncError::new("transfer-target-mismatch", 502));
                        }
                        let bases = recipe
                            .bases
                            .iter()
                            .map(|b| self.cache.read(&b.hash, delta::MAX_TARGET_BYTES))
                            .collect::<Result<Vec<_>>>()?;
                        let bytes =
                            recipe.apply(&bases.iter().map(Vec::as_slice).collect::<Vec<_>>())?;
                        self.cache.put(&bytes)?;
                    }
                    Frame::FullRequired { hash, size } => {
                        if hash != *target {
                            return Err(SyncError::new("transfer-target-mismatch", 502));
                        }
                        self.download_large(target, size)?;
                    }
                }
            }
        }
        Ok(())
    }
    fn upload_large(&self, hash: &str, size: u64) -> Result<()> {
        let cached: Option<(String, String)> = self
            .db
            .query_row("SELECT id,size FROM uploads WHERE hash=?1", [hash], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        let mut id = cached
            .filter(|(_, s)| s == &size.to_string())
            .map(|(id, _)| id);
        let mut verified = BTreeSet::new();
        let mut after = None;
        if let Some(upload_id) = id.as_ref() {
            loop {
                let query = after
                    .as_ref()
                    .map(|s: &Sequence| vec![("after", s.as_str().to_owned())])
                    .unwrap_or_default();
                match self.client.json::<UploadProgress>(
                    Method::GET,
                    &format!("uploads/{upload_id}"),
                    &query,
                    None::<&()>,
                    &[],
                ) {
                    Ok((_, progress)) => {
                        if progress.upload_id != *upload_id
                            || progress.manifest.hash != hash
                            || progress.manifest.size != Sequence::from(size)
                            || progress.chunk_bytes != Sequence::from(CHUNK as u64)
                        {
                            return Err(SyncError::new("upload-manifest-mismatch", 409));
                        }
                        if progress.complete {
                            self.db
                                .execute("DELETE FROM uploads WHERE hash=?1", [hash])?;
                            return Ok(());
                        }
                        if progress.finishing || progress.failure.is_some() {
                            return self.wait_upload(upload_id, hash, size);
                        }
                        for part in progress.verified {
                            verified.insert(
                                part.as_str()
                                    .parse::<u64>()
                                    .map_err(|_| SyncError::new("invalid-chunk-index", 502))?,
                            );
                        }
                        if progress.next_after.is_none() {
                            break;
                        }
                        if after == progress.next_after {
                            return Err(SyncError::new("invalid-upload-cursor", 502));
                        }
                        after = progress.next_after;
                    }
                    Err(error) if [404, 410].contains(&error.status) => {
                        id = None;
                        verified.clear();
                        break;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        let id = if let Some(id) = id {
            id
        } else {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase", deny_unknown_fields)]
            struct Started {
                upload_id: String,
            }
            let (_, started): (_, Started) = self.client.json(
                Method::POST,
                "uploads",
                &[],
                Some(&serde_json::json!({"hash":hash,"size":size.to_string()})),
                &[],
            )?;
            risunest_sync_wire::validate_id(&started.upload_id)?;
            self.db.execute("INSERT INTO uploads VALUES(?1,?2,?3) ON CONFLICT(hash) DO UPDATE SET id=excluded.id,size=excluded.size",params![hash,started.upload_id,size.to_string()])?;
            started.upload_id
        };
        let mut file = self
            .cache
            .cas
            .open_object(hash)?
            .ok_or_else(|| SyncError::new("cached-object-missing", 409))?;
        for index in 0..size.div_ceil(CHUNK as u64) {
            if verified.contains(&index) {
                continue;
            }
            let offset = index * CHUNK as u64;
            file.seek(SeekFrom::Start(offset))?;
            let mut bytes = vec![0; ((size - offset).min(CHUNK as u64)) as usize];
            file.read_exact(&mut bytes)?;
            let reply = self.client.request(
                Method::PUT,
                &format!("uploads/{id}/chunks/{index}"),
                &[],
                Some(bytes.clone()),
                &[("x-content-sha256", risunest_sync_wire::hash(&bytes))],
                MAX_METADATA_BYTES,
            )?;
            if reply.status != 204 {
                return Err(response_error(reply));
            }
        }
        let (status, result): (_, serde_json::Value) = self.client.json(
            Method::POST,
            &format!("uploads/{id}/complete"),
            &[],
            None::<&()>,
            &[],
        )?;
        if status == 202 {
            if result.get("uploadId").and_then(|v| v.as_str()) != Some(id.as_str())
                || result.get("status").and_then(|v| v.as_str()) != Some("pending")
            {
                return Err(SyncError::new("upload-target-mismatch", 502));
            }
            return self.wait_upload(&id, hash, size);
        }
        if result.get("hash").and_then(|h| h.as_str()) != Some(hash) {
            return Err(SyncError::new("upload-target-mismatch", 502));
        }
        self.db
            .execute("DELETE FROM uploads WHERE hash=?1", [hash])?;
        Ok(())
    }
    fn wait_upload(&self, id: &str, hash: &str, size: u64) -> Result<()> {
        loop {
            let (_, progress): (_, UploadProgress) =
                self.client
                    .json(Method::GET, &format!("uploads/{id}"), &[], None::<&()>, &[])?;
            if progress.upload_id != id
                || progress.manifest.hash != hash
                || progress.manifest.size != Sequence::from(size)
                || progress.chunk_bytes != Sequence::from(CHUNK as u64)
            {
                return Err(SyncError::new("upload-manifest-mismatch", 409));
            }
            if progress.failure.is_some() {
                let reply = self.client.request(
                    Method::DELETE,
                    &format!("uploads/{id}"),
                    &[],
                    None,
                    &[],
                    MAX_METADATA_BYTES,
                )?;
                if reply.status == 204 || reply.status == 404 || reply.status == 410 {
                    self.db
                        .execute("DELETE FROM uploads WHERE hash=?1", [hash])?;
                }
                return Err(SyncError::new("upload-finalization-failed", 409));
            }
            if progress.complete {
                self.db
                    .execute("DELETE FROM uploads WHERE hash=?1", [hash])?;
                return Ok(());
            }
            if !progress.finishing {
                return Err(SyncError::new("upload-finalization-interrupted", 409));
            }
            self.client.ensure_active()?;
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }
    fn download_large(&self, target: &str, size: u64) -> Result<()> {
        if size > 1024 * 1024 * 1024 * 1024 {
            return Err(SyncError::new("object-too-large", 413));
        }
        for index in 0..size.div_ceil(CHUNK as u64) {
            let offset = index * CHUNK as u64;
            let length = (size - offset).min(CHUNK as u64);
            let cached: Option<(String, i64)> = self
                .db
                .query_row(
                    "SELECT hash,size FROM chunks WHERE target=?1 AND part=?2",
                    params![target, index as i64],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((hash, stored)) = cached {
                if stored >= 0 && stored as u64 == length && self.cache.read(&hash, CHUNK).is_ok() {
                    continue;
                }
            }
            let end = offset + length - 1;
            let reply = self.client.request(
                Method::GET,
                &format!("objects/{target}"),
                &[],
                None,
                &[
                    ("range", format!("bytes={offset}-{end}")),
                    ("if-range", format!("\"{target}\"")),
                ],
                CHUNK,
            )?;
            if reply.status != 206 {
                return Err(response_error(reply));
            }
            if reply.content_range.as_deref() != Some(&format!("bytes {offset}-{end}/{size}"))
                || reply.body.len() as u64 != length
            {
                return Err(SyncError::new("invalid-object-range", 502));
            }
            let hash = self.cache.put(&reply.body)?;
            self.db.execute("INSERT INTO chunks VALUES(?1,?2,?3,?4) ON CONFLICT(target,part) DO UPDATE SET hash=excluded.hash,size=excluded.size",params![target,index as i64,hash,length as i64])?;
        }
        let mut reader = ChunkReader {
            transfer: self,
            target,
            index: 0,
            count: size.div_ceil(CHUNK as u64),
            chunk: std::io::Cursor::new(Vec::new()),
        };
        self.cache
            .cas
            .prepare_reader_expected(&mut reader, target, size)?;
        self.db
            .execute("DELETE FROM chunks WHERE target=?1", [target])?;
        Ok(())
    }
}
struct ChunkReader<'a, 'b> {
    transfer: &'a Transfer<'b>,
    target: &'a str,
    index: u64,
    count: u64,
    chunk: std::io::Cursor<Vec<u8>>,
}
impl Read for ChunkReader<'_, '_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            let n = self.chunk.read(buffer)?;
            if n != 0 || self.index == self.count {
                return Ok(n);
            }
            let hash: String = self
                .transfer
                .db
                .query_row(
                    "SELECT hash FROM chunks WHERE target=?1 AND part=?2",
                    params![self.target, self.index as i64],
                    |r| r.get(0),
                )
                .map_err(|_| std::io::Error::other("Missing verified chunk"))?;
            self.chunk = std::io::Cursor::new(
                self.transfer
                    .cache
                    .read(&hash, CHUNK)
                    .map_err(|_| std::io::Error::other("Invalid verified chunk"))?,
            );
            self.index += 1;
        }
    }
}
