use super::{uploads::now, Device, Store};
use crate::{Error, Result};
use risunest_sync_wire::{
    canonical,
    delta::{self, Recipe},
    hash,
    transfer::{self, Frame},
    validate_hash,
};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferRequest {
    pub target: String,
    pub bases: Vec<String>,
}
impl Store {
    pub fn receive_frames(&self, device: &Device, bytes: &[u8]) -> Result<Vec<String>> {
        let frames = transfer::decode(bytes)?;
        if frames
            .iter()
            .any(|f| matches!(f, Frame::FullRequired { .. }))
        {
            return Err(Error::new("invalid-upload-frame", 400));
        }
        let mut verified = Vec::new();
        for frame in frames {
            let bytes = match frame {
                Frame::Full(bytes) => bytes,
                Frame::Delta(recipe) => {
                    let hashes = recipe
                        .bases
                        .iter()
                        .map(|b| b.hash.clone())
                        .collect::<Vec<_>>();
                    self.pin_objects(device, &hashes)?;
                    let bases = hashes
                        .iter()
                        .map(|h| self.get_object(h))
                        .collect::<Result<Vec<_>>>()?;
                    recipe.apply(&bases.iter().map(Vec::as_slice).collect::<Vec<_>>())?
                }
                Frame::FullRequired { .. } => unreachable!(),
            };
            let digest = hash(&bytes);
            self.put_object(device, &digest, &bytes)?;
            verified.push(digest);
        }
        Ok(verified)
    }
    pub fn transfer_objects(
        &self,
        device: &Device,
        requests: &[TransferRequest],
    ) -> Result<Vec<u8>> {
        if requests.len() > 1024 {
            return Err(Error::new("too-many-candidates", 400));
        }
        let mut frames = Vec::new();
        let mut used = 8usize;
        let mut materialized = 0u64;
        for request in requests {
            validate_hash(&request.target)?;
            if request.bases.len() > delta::MAX_BASES {
                return Err(Error::new("too-many-bases", 400));
            }
            let mut seen = std::collections::BTreeSet::new();
            for base in &request.bases {
                validate_hash(base)?;
                if !seen.insert(base) {
                    return Err(Error::new("duplicate-base", 400));
                }
            }
            self.pin_objects(device, std::slice::from_ref(&request.target))?;
            let size = self
                .object_size(&request.target)?
                .ok_or(Error::new("object-not-found", 404))?;
            let mut frame = Frame::FullRequired {
                hash: request.target.clone(),
                size,
            };
            if size <= delta::MAX_TARGET_BYTES as u64 && materialized + size <= 32 * 1024 * 1024 {
                let target = self.get_object(&request.target)?;
                let mut bases = Vec::new();
                let mut base_bytes = 0;
                for digest in request.bases.iter().filter(|_| size > 64) {
                    if let Some(size) = self.object_size(digest)? {
                        if size <= delta::MAX_TARGET_BYTES as u64
                            && base_bytes + size <= delta::MAX_BASE_BYTES as u64
                        {
                            self.pin_objects(device, std::slice::from_ref(digest))?;
                            bases.push(self.get_object(digest)?);
                            base_bytes += size;
                        }
                    }
                }
                let recipe_key = hash(&canonical::encode(
                    &serde_json::json!({"target":request.target,"bases":bases.iter().map(|b|hash(b)).collect::<Vec<_>>()}),
                )?);
                let cached: Option<Vec<u8>> = self
                    .reader()?
                    .query_row(
                        "SELECT body FROM transfer_recipes WHERE id=?1 AND expires>?2",
                        params![recipe_key, now()?],
                        |r| r.get(0),
                    )
                    .optional()?;
                let recipe = if let Some(bytes) = cached {
                    Some(Recipe::decode(&bytes)?)
                } else if bases.is_empty() {
                    None
                } else {
                    delta::create(
                        &bases.iter().map(Vec::as_slice).collect::<Vec<_>>(),
                        &target,
                    )
                    .ok()
                };
                if let Some(recipe) = recipe {
                    if let Ok(bytes) = recipe.encode() {
                        if bytes.len() + 64 < target.len()
                            && used + bytes.len() + 5 <= 8 * 1024 * 1024
                        {
                            let db = self.db()?;
                            db.execute("DELETE FROM transfer_recipes WHERE expires<=?1", [now()?])?;
                            let cache_bytes: i64 = db.query_row(
                                "SELECT coalesce(sum(length(body)),0) FROM transfer_recipes",
                                [],
                                |r| r.get(0),
                            )?;
                            if cache_bytes + bytes.len() as i64 <= 64 * 1024 * 1024 {
                                db.execute("INSERT INTO transfer_recipes VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET expires=excluded.expires",params![recipe_key,bytes,now()?+3600])?;
                            }
                            frame = Frame::Delta(recipe);
                        }
                    }
                }
                if matches!(frame, Frame::FullRequired { .. })
                    && used + target.len() + 45 <= transfer::PREFERRED_BATCH_BYTES
                {
                    frame = Frame::Full(target);
                }
            }
            let encoded = transfer::encode(std::slice::from_ref(&frame))?;
            if used + encoded.len() - 8 > 8 * 1024 * 1024 {
                return Err(Error::new("batch-too-large", 413));
            }
            used += encoded.len() - 8;
            if !matches!(frame, Frame::FullRequired { .. }) {
                materialized += size;
            }
            frames.push(frame);
        }
        Ok(transfer::encode(&frames)?)
    }
}
