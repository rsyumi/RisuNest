use super::{Device, Store};
use crate::{Error, Result};
use risunest_sync_connect::media::{
    MediaAccess, MediaClaims, MediaRequest, ACCESS_LIFETIME_SECONDS,
};
use rusqlite::OptionalExtension;

pub enum MediaResponse {
    Refresh(String),
    File {
        file: std::fs::File,
        size: u64,
        hash: String,
        mime: String,
        max_age: u64,
    },
}

impl Store {
    pub fn media_access(
        &self,
        device: &Device,
        epoch: &str,
        requests: &[MediaRequest],
    ) -> Result<Vec<MediaAccess>> {
        if requests.len() > 128 {
            return Err(Error::new("too-many-media-objects", 400));
        }
        let _gate = self
            .objects_gate
            .lock()
            .map_err(|_| Error::new("storage-unavailable", 503))?;
        let db = self.reader()?;
        Self::require_device(&db, device)?;
        let head = Self::read_head(&db)?;
        if head.epoch != epoch {
            return Err(Error::new("epoch-mismatch", 409));
        }
        let expires = super::uploads::now()? as u64 + ACCESS_LIFETIME_SECONDS;
        let mut grants = Vec::with_capacity(requests.len());
        for request in requests {
            request.validate()?;
            let size: Option<i64> = db
                .query_row(
                    "SELECT size FROM objects WHERE hash=?1",
                    [&request.object.hash],
                    |r| r.get(0),
                )
                .optional()?;
            let size = u64::try_from(size.ok_or(Error::new("object-not-found", 404))?)
                .map_err(|_| Error::new("corrupt-object", 503))?;
            if request.object.size != size.into() {
                return Err(Error::new("object-size-mismatch", 409));
            }
            let metadata = std::fs::metadata(self.object_path(&request.object.hash)?)?;
            if !metadata.is_file() || metadata.len() != size {
                return Err(Error::new("corrupt-object", 503));
            }
            let claims = MediaClaims {
                library_id: head.library_id.clone(),
                epoch: head.epoch.clone(),
                device_id: device.id.clone(),
                request: request.clone(),
                expires: expires.into(),
            };
            grants.push(MediaAccess {
                object: request.object.clone(),
                token: self.media_signer.sign_access(&claims)?,
                valid_for_seconds: ACCESS_LIFETIME_SECONDS.into(),
            });
        }
        Ok(grants)
    }

    pub fn resolve_media(&self, token: &str) -> Result<MediaResponse> {
        let claims = self
            .media_signer
            .verify_access(token)
            .map_err(|_| Error::new("invalid-media-capability", 403))?;
        {
            let db = self.reader()?;
            let head = Self::read_head(&db)?;
            if claims.library_id != head.library_id || claims.epoch != head.epoch {
                return Err(Error::new("media-identity-mismatch", 403));
            }
            Self::require_device(
                &db,
                &Device {
                    id: claims.device_id.clone(),
                },
            )
            .map_err(|_| Error::new("media-device-revoked", 403))?;
        }
        let now = super::uploads::now()? as u64;
        let expires = claims
            .expires
            .as_str()
            .parse::<u64>()
            .map_err(|_| Error::new("invalid-media-expiry", 403))?;
        if expires <= now {
            // Only an authentic expired capability reaches this redirect. The
            // native endpoint must verify its independent per-process signature
            // and reacquire authorization; expiration never serves file bytes.
            return Ok(MediaResponse::Refresh(claims.request.refresh_url));
        }
        let (file, size) = self.open_object(&claims.request.object.hash)?;
        if claims.request.object.size != size.into() {
            return Err(Error::new("object-size-mismatch", 409));
        }
        Ok(MediaResponse::File {
            file,
            size,
            hash: claims.request.object.hash,
            mime: claims.request.object.mime,
            max_age: (expires - now).min(ACCESS_LIFETIME_SECONDS),
        })
    }
}

#[cfg(test)]
mod tests;
