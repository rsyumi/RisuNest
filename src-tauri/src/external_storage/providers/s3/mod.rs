//! S3-compatible external storage: one signing and transfer core with a preset
//! per service in `profiles/`.
//!
//! # Connection
//!
//! - `provider` is `s3` and `oauth_profile` must be absent; the service
//!   authenticates with static access keys, not an authorization code flow.
//! - `profile` is `aws`, `r2`, `b2`, `hf` or `generic` and selects the preset.
//! - `endpoint` is `https://<host>` with an optional base path. Amazon S3 uses
//!   `https://s3.<region>.amazonaws.com`, Cloudflare R2
//!   `https://<account>.r2.cloudflarestorage.com`, Backblaze B2
//!   `https://s3.<region>.backblazeb2.com`, Hugging Face Storage Buckets
//!   `https://s3.hf.co/<namespace>`, where the single path segment is required.
//!   Plain `http` is accepted only for `127.0.0.1`, which is the loopback wire
//!   fixture; the product transport refuses any other plain-text endpoint.
//! - `account_id` is the access key ID, or whatever account identity the
//!   service shares its request quota by. It keys the durable budget and is not
//!   part of the repository identity, so a second device may use its own key
//!   for the same bucket.
//! - `location` accepts exactly `bucket`, `prefix`, `region` and `addressing`.
//!   `prefix` may be absent or empty for the bucket root. `region` is required
//!   unless the preset fixes one (`us-east-1` for Hugging Face). `addressing`
//!   is `path` or `virtual` and defaults per preset; Hugging Face serves path
//!   addressing only.
//!
//! # Secret payload
//!
//! The vault holds one UTF-8 JSON object and nothing else:
//!
//! ```text
//! {"accessKeyId": "...", "secretAccessKey": "..."}
//! ```
//!
//! Unknown fields are rejected. An absent, unreadable or malformed payload is
//! reported as re-authentication rather than a transport failure.
//!
//! # Remote layout
//!
//! Under `<prefix>`: `head` (and `heads/<name>`) for head objects,
//! `descriptors/<object_id>`, `packs/<object_id>`, `catalogs/<object_id>`,
//! `snapshots/<object_id>` and `backup-points/<object_id>` for the object
//! roles. A locator's `object` is the key relative to the prefix, never a
//! download or session URL.
use super::Dependencies;
use crate::external_storage::contract::{Provider, Result};
use std::sync::Arc;

mod config;
mod profiles;
mod provider;
mod sigv4;
mod xml;

pub(crate) fn create(dependencies: Dependencies) -> Result<Arc<dyn Provider>> {
    provider::create(dependencies)
}

#[cfg(test)]
mod tests;
