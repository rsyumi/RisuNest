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
const CHUNK: usize = transfer::UPLOAD_CHUNK_BYTES;
#[path = "transfer_references.rs"]
mod references;

/// Resume metadata contains only content identities and server staging IDs.
/// Every resumed chunk lives in the verified cache CAS before its row is saved.
pub(crate) struct Transfer<'a> {
    pub client: &'a ServerClient,
    pub cache: &'a Cache,
    check: Option<&'a dyn Fn() -> Result<()>>,
    db: Connection,
    base_sizes: std::cell::RefCell<Option<(Vec<String>, Vec<(String, u64)>)>>,
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
        base_version: &risunest_sync_wire::RecordVersion,
    ) -> Result<Vec<String>> {
        self.download_record_inner(version, bases, base_version, false)
    }
    pub fn download_record_metadata(
        &self,
        version: &risunest_sync_wire::RecordVersion,
        bases: &[String],
        base_version: &risunest_sync_wire::RecordVersion,
    ) -> Result<Vec<String>> {
        self.download_record_inner(version, bases, base_version, true)
    }
    fn download_record_inner(
        &self,
        version: &risunest_sync_wire::RecordVersion,
        bases: &[String],
        base_version: &risunest_sync_wire::RecordVersion,
        metadata_only: bool,
    ) -> Result<Vec<String>> {
        use risunest_sync_wire::descriptor::RecordDescriptor;
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
        let previous = if let risunest_sync_wire::RecordVersion::Live {
            descriptor_hash: Some(hash),
            ..
        } = base_version
        {
            self.cache
                .read(hash, MAX_METADATA_BYTES)
                .ok()
                .and_then(|bytes| {
                    canonical::decode::<RecordDescriptor>(&bytes, MAX_METADATA_BYTES).ok()
                })
                .filter(|d| d.validate().is_ok())
        } else {
            None
        };
        let roots = [
            (
                descriptor.dependency_root,
                previous.as_ref().and_then(|d| d.dependency_root.clone()),
            ),
            (
                descriptor.relation_root,
                previous.and_then(|d| d.relation_root),
            ),
        ]
        .into_iter()
        .filter_map(|(root, base)| root.map(|root| (root, base.into_iter().collect())))
        .collect();
        let dependencies = self.download_reference_tree(roots)?;
        if metadata_only {
            let (payload, _) = self.cache.restore_with(version, |hash, limit| {
                self.download(&[hash.to_owned()], bases)?;
                self.cache.read(hash, limit)
            })?;
            use crate::logical_records::LogicalRecordEnvelope as Envelope;
            let local = match &payload.record {
                Envelope::Root { owner_heads, .. } | Envelope::Character { owner_heads, .. } => {
                    owner_heads
                        .iter()
                        .filter_map(|h| h.manifest_hash.clone())
                        .collect()
                }
                Envelope::Cold { object_hash, .. } => object_hash.iter().cloned().collect(),
                _ => Vec::new(),
            };
            self.download(&local, bases)?;
        } else {
            self.download(&dependencies, bases)?;
        }
        self.cache.closure(version)
    }
    pub fn new(client: &'a ServerClient, cache: &'a Cache) -> Result<Self> {
        let db = Connection::open(cache.cas.repository_root().join("transfers.sqlite"))?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS uploads(hash TEXT PRIMARY KEY,id TEXT NOT NULL,size TEXT NOT NULL); CREATE TABLE IF NOT EXISTS chunks(target TEXT NOT NULL,part INTEGER NOT NULL,hash TEXT NOT NULL,size INTEGER NOT NULL,PRIMARY KEY(target,part));")?;
        Ok(Self {
            client,
            cache,
            check: None,
            db,
            base_sizes: std::cell::RefCell::new(None),
        })
    }
    /// Job cancellation is borrowed by the coordinating thread. Chunk workers
    /// finish their bounded requests before the next batch is admitted.
    pub(crate) fn with_check(mut self, check: &'a dyn Fn() -> Result<()>) -> Self {
        self.check = Some(check);
        self
    }
    fn ensure_active(&self) -> Result<()> {
        self.client.ensure_active()?;
        if let Some(check) = self.check {
            check()?;
        }
        Ok(())
    }
    #[cfg(test)]
    pub fn upload(&self, hashes: &[String], base_candidates: &[String]) -> Result<()> {
        self.upload_with_leased_bases(hashes, base_candidates, false)
    }
    /// The caller may reuse a successfully renewed committed descriptor lease.
    /// It protects every candidate in that descriptor's verified closure.
    #[cfg(test)]
    pub fn upload_with_leased_bases(
        &self,
        hashes: &[String],
        base_candidates: &[String],
        base_lease: bool,
    ) -> Result<()> {
        self.upload_with_hints(
            hashes,
            base_candidates,
            base_lease,
            &std::collections::BTreeMap::new(),
        )
    }
    pub(crate) fn upload_with_hints(
        &self,
        hashes: &[String],
        base_candidates: &[String],
        base_lease: bool,
        hints: &std::collections::BTreeMap<String, Vec<String>>,
    ) -> Result<()> {
        for page in hashes.chunks(1024) {
            self.ensure_active()?;
            let mut descriptors = Vec::with_capacity(page.len());
            let mut sizes = std::collections::BTreeMap::new();
            for hash in page {
                let size = self
                    .cache
                    .cas
                    .stat_object(hash)?
                    .ok_or_else(|| SyncError::new("cached-object-missing", 409))?;
                descriptors.push(serde_json::json!({"hash":hash,"size":size.to_string()}));
                sizes.insert(hash.as_str(), size);
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
            let mut candidates = BTreeSet::new();
            for target in missing.missing.iter().filter(|_| !base_lease) {
                let base_candidates = hints.get(target).map_or(base_candidates, Vec::as_slice);
                let size = sizes[target.as_str()];
                candidates.extend(if size > delta::MAX_TARGET_BYTES as u64 {
                    self.large_bases(target, size, base_candidates)?
                } else {
                    self.select_bases(target, Some(size), base_candidates)?
                });
            }
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
                self.ensure_active()?;
                let base_candidates = hints.get(target).map_or(base_candidates, Vec::as_slice);
                let size = sizes[target.as_str()];
                if size > delta::MAX_TARGET_BYTES as u64 {
                    self.upload_large(target, size, base_candidates)?;
                    self.client.verified(size);
                    continue;
                }
                let bytes = self.cache.read(target, delta::MAX_TARGET_BYTES)?;
                let candidates = self.select_bases(target, Some(size), base_candidates)?;
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
                        self.upload_large(target, size, base_candidates)?;
                        self.client.verified(size);
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                };
                if length + 8 > transfer::PREFERRED_BATCH_BYTES && matches!(frame, Frame::Full(_)) {
                    self.upload_large(target, size, base_candidates)?;
                    self.client.verified(size);
                    continue;
                }
                if used + length > transfer::PREFERRED_BATCH_BYTES
                    || materialized + size as usize > 32 * 1024 * 1024
                {
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
        self.ensure_active()
    }
    fn send_frames(&self, frames: &[Frame]) -> Result<()> {
        self.ensure_active()?;
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
        for frame in frames {
            self.client.verified(match frame {
                Frame::Full(bytes) => bytes.len() as u64,
                Frame::Delta(recipe) => recipe.target_size,
                Frame::FullRequired { .. } => 0,
            });
        }
        self.ensure_active()
    }
    pub fn pin(&self, hashes: &[String]) -> Result<()> {
        for page in hashes.chunks(1024) {
            self.ensure_active()?;
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
        self.ensure_active()
    }
    fn select_bases(
        &self,
        target: &str,
        target_size: Option<u64>,
        candidates: &[String],
    ) -> Result<Vec<String>> {
        let mut result = Vec::new();
        let mut total = 0;
        // The minimum useful recipe saving is 64 bytes, so no base can help a
        // smaller full object. Avoid repeatedly opening unrelated metadata bases.
        if target_size.is_some_and(|size| size <= 64) {
            return Ok(Vec::new());
        }
        let mut ranked = Vec::new();
        for (candidate, size) in self.candidate_sizes(candidates)? {
            if size <= delta::MAX_TARGET_BYTES as u64 {
                ranked.push((candidate, size));
            }
        }
        ranked.sort_by_key(|(_, size)| {
            target_size
                .map(|target| target.abs_diff(*size))
                .unwrap_or(u64::MAX - *size)
        });
        for (candidate, size) in ranked {
            if candidate == target || result.contains(&candidate) {
                continue;
            }
            if size <= delta::MAX_TARGET_BYTES as u64
                && total + size <= delta::MAX_BASE_BYTES as u64
            {
                result.push(candidate);
                total += size;
                if result.len() == delta::MAX_BASES {
                    break;
                }
            }
        }
        Ok(result)
    }
    fn candidate_sizes(&self, candidates: &[String]) -> Result<Vec<(String, u64)>> {
        if let Some((keys, sizes)) = &*self.base_sizes.borrow() {
            if keys == candidates {
                return Ok(sizes.clone());
            }
        }
        let mut sizes = Vec::new();
        for candidate in candidates {
            if let Some(size) = self.cache.cas.stat_object(candidate)? {
                sizes.push((candidate.clone(), size));
            }
        }
        // Only one record's immutable base inventory is retained. Missing entries
        // are not cached, since a subsequent download may make them available.
        if sizes.len() == candidates.len() {
            *self.base_sizes.borrow_mut() = Some((candidates.to_vec(), sizes.clone()));
        }
        Ok(sizes)
    }
    pub fn download(&self, hashes: &[String], base_candidates: &[String]) -> Result<()> {
        self.download_with_hints(hashes, base_candidates, &std::collections::BTreeMap::new())
    }
    fn download_with_hints(
        &self,
        hashes: &[String],
        base_candidates: &[String],
        hints: &std::collections::BTreeMap<String, Vec<String>>,
    ) -> Result<()> {
        // Page the absent targets, not the full inventory. A few changed hashes
        // scattered among 100k cached objects still belong to one request.
        let mut absent = hashes.iter().filter_map(|h| {
            if let Err(error) = self.ensure_active() {
                return Some(Err(error));
            }
            match self.cache.cas.stat_object(h) {
                Ok(Some(_)) => None,
                Ok(None) => Some(Ok(h.clone())),
                Err(e) => Some(Err(SyncError::from(e))),
            }
        });
        loop {
            let missing = absent.by_ref().take(1024).collect::<Result<Vec<_>>>()?;
            if missing.is_empty() {
                break;
            }
            let mut requests = Vec::new();
            for target in &missing {
                let candidates = hints.get(target).map_or(base_candidates, Vec::as_slice);
                requests.push(serde_json::json!({"target":target,"bases":self.select_bases(target,None,candidates)?}));
            }
            let reply = self.client.request(
                Method::POST,
                "objects/transfer",
                &[],
                Some(canonical::encode(&requests)?),
                &[],
                risunest_sync_wire::batch::MAX_BATCH_BYTES,
            )?;
            if reply.status != 200 {
                return Err(response_error(reply));
            }
            let frames = transfer::decode(&reply.body)?;
            if frames.len() != missing.len() {
                return Err(SyncError::new("transfer-count-mismatch", 502));
            }
            for (target, frame) in missing.iter().zip(frames) {
                self.ensure_active()?;
                match frame {
                    Frame::Full(bytes) => {
                        if hash(&bytes) != *target {
                            return Err(SyncError::new("transfer-target-mismatch", 502));
                        }
                        self.cache.put(&bytes)?;
                        self.client.verified(bytes.len() as u64);
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
                        self.client.verified(bytes.len() as u64);
                    }
                    Frame::FullRequired { hash, size } => {
                        if hash != *target {
                            return Err(SyncError::new("transfer-target-mismatch", 502));
                        }
                        let candidates = hints.get(target).map_or(base_candidates, Vec::as_slice);
                        if !self.download_delta(target, size, candidates)? {
                            self.download_large(target, size)?;
                        }
                        self.client.verified(size);
                    }
                }
            }
        }
        self.ensure_active()
    }
    fn large_bases(&self, target: &str, size: u64, candidates: &[String]) -> Result<Vec<String>> {
        let mut ranked = Vec::new();
        for (candidate, bytes) in self.candidate_sizes(candidates)? {
            if candidate == target {
                continue;
            }
            if bytes > delta::MAX_TARGET_BYTES as u64
                && bytes <= risunest_sync_wire::stream_delta::MAX_FILE_BYTES
            {
                ranked.push((candidate, bytes));
            }
        }
        ranked.sort_by_key(|(_, bytes)| size.abs_diff(*bytes));
        let mut total = 0;
        let mut result = Vec::new();
        for (digest, bytes) in ranked {
            if result.contains(&digest)
                || total + bytes > risunest_sync_wire::stream_delta::MAX_FILE_BYTES
            {
                continue;
            }
            total += bytes;
            result.push(digest);
            if result.len() == delta::MAX_BASES {
                break;
            }
        }
        Ok(result)
    }
    fn upload_delta(
        &self,
        id: &str,
        target: &str,
        size: u64,
        candidates: &[String],
    ) -> Result<bool> {
        use risunest_sync_wire::{delta::Base, stream_delta, WireError};
        let candidates = self.large_bases(target, size, candidates)?;
        if candidates.is_empty() {
            return Ok(false);
        }
        let mut sources = candidates
            .iter()
            .map(|h| {
                self.cache
                    .cas
                    .open_object(h)?
                    .ok_or_else(|| SyncError::new("cached-object-missing", 409))
            })
            .collect::<Result<Vec<_>>>()?;
        let identities = candidates
            .iter()
            .zip(&sources)
            .map(|(hash, file)| {
                Ok(Base {
                    hash: hash.clone(),
                    size: file.metadata()?.len(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut file = self
            .cache
            .cas
            .open_object(target)?
            .ok_or_else(|| SyncError::new("cached-object-missing", 409))?;
        let started = std::time::Instant::now();
        let recipe = stream_delta::create(
            &mut sources,
            &identities,
            &mut file,
            Base {
                hash: target.into(),
                size,
            },
            || {
                self.ensure_active().map_err(|_| WireError("cancelled"))?;
                if started.elapsed() > std::time::Duration::from_secs(120) {
                    return Err(WireError("delta-budget"));
                }
                Ok(())
            },
        );
        let recipe = match recipe {
            Ok(recipe) => recipe,
            Err(WireError("delta-limit" | "delta-budget")) => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let reply = self.client.request(
            Method::PUT,
            &format!("uploads/{id}/delta"),
            &[],
            Some(stream_delta::encode(&recipe)?),
            &[],
            MAX_METADATA_BYTES,
        )?;
        match reply.status {
            202 => Ok(true),
            404 => Ok(false),
            _ => Err(response_error(reply)),
        }
    }
    fn download_delta(&self, target: &str, size: u64, candidates: &[String]) -> Result<bool> {
        use risunest_sync_wire::{stream_delta, WireError};
        if size <= delta::MAX_TARGET_BYTES as u64 {
            return Ok(false);
        }
        let bases = self.large_bases(target, size, candidates)?;
        if bases.is_empty() {
            return Ok(false);
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Started {
            job_id: String,
        }
        let (_, started): (_, Started) = self.client.json(
            Method::POST,
            "objects/delta",
            &[],
            Some(&serde_json::json!({"target":target,"bases":bases})),
            &[],
        )?;
        risunest_sync_wire::validate_id(&started.job_id)?;
        let path = format!("object-deltas/{}", started.job_id);
        loop {
            self.ensure_active()?;
            let reply = self.client.request(
                Method::GET,
                &path,
                &[("wait", "true".into())],
                None,
                &[],
                risunest_sync_wire::batch::MAX_BATCH_BYTES,
            )?;
            self.ensure_active()?;
            match reply.status {
                202 => continue,
                204 => {
                    self.client.request(
                        Method::DELETE,
                        &path,
                        &[],
                        None,
                        &[],
                        MAX_METADATA_BYTES,
                    )?;
                    return Ok(false);
                }
                200 => {
                    let recipe = stream_delta::decode(&reply.body)?;
                    if recipe.target_hash != target
                        || recipe.target_size != size
                        || recipe.bases.iter().any(|b| !bases.contains(&b.hash))
                    {
                        return Err(SyncError::new("transfer-target-mismatch", 502));
                    }
                    let mut sources = recipe
                        .bases
                        .iter()
                        .map(|b| {
                            self.cache
                                .cas
                                .open_object(&b.hash)?
                                .ok_or_else(|| SyncError::new("cached-object-missing", 409))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    let mut temporary =
                        tempfile::NamedTempFile::new_in(self.cache.cas.repository_root())?;
                    stream_delta::apply(&recipe, &mut sources, &mut temporary, || {
                        self.ensure_active().map_err(|_| WireError("cancelled"))
                    })?;
                    temporary.seek(SeekFrom::Start(0))?;
                    prepare_checked(&self.cache.cas, &mut temporary, target, size, &|| {
                        self.ensure_active()
                    })?;
                    let released = self.client.request(
                        Method::DELETE,
                        &path,
                        &[],
                        None,
                        &[],
                        MAX_METADATA_BYTES,
                    )?;
                    if released.status != 204 {
                        return Err(response_error(released));
                    }
                    return Ok(true);
                }
                _ => return Err(response_error(reply)),
            }
        }
    }
    fn upload_large(&self, hash: &str, size: u64, base_candidates: &[String]) -> Result<()> {
        self.ensure_active()?;
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
                self.ensure_active()?;
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
        if verified.is_empty()
            && size > delta::MAX_TARGET_BYTES as u64
            && self.upload_delta(&id, hash, size, base_candidates)?
        {
            return self.wait_upload(&id, hash, size);
        }
        let mut file = self
            .cache
            .cas
            .open_object(hash)?
            .ok_or_else(|| SyncError::new("cached-object-missing", 409))?;
        let mut missing = (0..size.div_ceil(CHUNK as u64)).filter(|i| !verified.contains(i));
        loop {
            let mut batch = Vec::with_capacity(2);
            for index in missing.by_ref().take(2) {
                self.ensure_active()?;
                let offset = index * CHUNK as u64;
                file.seek(SeekFrom::Start(offset))?;
                let mut bytes = vec![0; ((size - offset).min(CHUNK as u64)) as usize];
                file.read_exact(&mut bytes)?;
                batch.push((format!("uploads/{id}/chunks/{index}"), bytes));
            }
            if batch.is_empty() {
                break;
            }
            let client = self.client;
            let results = std::thread::scope(|scope| {
                let workers: Vec<_> = batch
                    .into_iter()
                    .map(|(path, bytes)| {
                        scope.spawn(move || {
                            let digest = risunest_sync_wire::hash(&bytes);
                            let reply = client.request(
                                Method::PUT,
                                &path,
                                &[],
                                Some(bytes),
                                &[("x-content-sha256", digest)],
                                MAX_METADATA_BYTES,
                            )?;
                            if reply.status != 204 {
                                return Err(response_error(reply));
                            }
                            Ok(())
                        })
                    })
                    .collect();
                workers
                    .into_iter()
                    .map(|worker| {
                        worker
                            .join()
                            .unwrap_or_else(|_| Err(SyncError::new("chunk-worker-failed", 500)))
                    })
                    .collect::<Vec<_>>()
            });
            // Join every request before returning. Accepted chunks remain in
            // the server bitmap even when the other response is lost.
            self.ensure_active()?;
            for result in results {
                result?;
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
            self.ensure_active()?;
            let (_, progress): (_, UploadProgress) = self.client.json(
                Method::GET,
                &format!("uploads/{id}"),
                &[("wait", "true".into())],
                None::<&()>,
                &[],
            )?;
            self.ensure_active()?;
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
            self.ensure_active()?;
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }
    fn download_large(&self, target: &str, size: u64) -> Result<()> {
        if size > 1024 * 1024 * 1024 * 1024 {
            return Err(SyncError::new("object-too-large", 413));
        }
        let mut indices = 0..size.div_ceil(CHUNK as u64);
        loop {
            self.ensure_active()?;
            let mut batch = Vec::with_capacity(2);
            for index in indices.by_ref() {
                self.ensure_active()?;
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
                    if stored >= 0
                        && stored as u64 == length
                        && self.cache.read(&hash, CHUNK).is_ok()
                    {
                        continue;
                    }
                }
                batch.push((index, offset, length));
                if batch.len() == 2 {
                    break;
                }
            }
            if batch.is_empty() {
                break;
            }
            let client = self.client;
            let results = std::thread::scope(|scope| {
                let workers: Vec<_> = batch
                    .into_iter()
                    .map(|(index, offset, length)| {
                        scope.spawn(move || {
                            let end = offset + length - 1;
                            let reply = client.request(
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
                            if reply.content_range.as_deref()
                                != Some(&format!("bytes {offset}-{end}/{size}"))
                                || reply.body.len() as u64 != length
                            {
                                return Err(SyncError::new("invalid-object-range", 502));
                            }
                            Ok((index, length, reply.body))
                        })
                    })
                    .collect();
                workers
                    .into_iter()
                    .map(|worker| {
                        worker
                            .join()
                            .unwrap_or_else(|_| Err(SyncError::new("chunk-worker-failed", 500)))
                    })
                    .collect::<Vec<_>>()
            });
            let mut failure = None;
            for result in results {
                let stored = result.and_then(|(index, length, bytes)| {
                    let hash = self.cache.put(&bytes)?;
                    self.db.execute("INSERT INTO chunks VALUES(?1,?2,?3,?4) ON CONFLICT(target,part) DO UPDATE SET hash=excluded.hash,size=excluded.size",params![target,index as i64,hash,length as i64])?;
                    Ok(())
                });
                if let Err(error) = stored {
                    failure.get_or_insert(error);
                }
            }
            // Persist all verified successes, including those after a failed
            // sibling, before the caller retries the remaining ranges.
            self.ensure_active()?;
            if let Some(error) = failure {
                return Err(error);
            }
        }
        let mut reader = ChunkReader {
            transfer: self,
            target,
            index: 0,
            count: size.div_ceil(CHUNK as u64),
            chunk: std::io::Cursor::new(Vec::new()),
        };
        prepare_checked(&self.cache.cas, &mut reader, target, size, &|| {
            self.ensure_active()
        })?;
        self.db
            .execute("DELETE FROM chunks WHERE target=?1", [target])?;
        Ok(())
    }
}

/// Preserve the job's cancellation error across the CAS streaming I/O boundary.
pub(crate) fn prepare_checked(
    cas: &crate::asset_repository::PayloadCas,
    reader: &mut impl Read,
    hash: &str,
    size: u64,
    check: &dyn Fn() -> Result<()>,
) -> Result<()> {
    struct Checked<'a, R> {
        reader: R,
        check: &'a dyn Fn() -> Result<()>,
    }
    impl<R: Read> Read for Checked<'_, R> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            (self.check)().map_err(|error| std::io::Error::other(error.code))?;
            self.reader.read(buffer)
        }
    }
    check()?;
    let outcome = cas.prepare_reader_expected(&mut Checked { reader, check }, hash, size);
    check()?;
    outcome?;
    Ok(())
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
