mod archive;
mod contract;
mod crypto;
mod loader;

pub use archive::{safe_archive_path, validate_archive_entry, ArchiveEntryKind};
pub use contract::{
    compare_versions, Architecture, Compatibility, Download, OperatingSystem, PackageFormat,
    PackageRequest, PlatformEntry, Product, ProductEntry, ProductRelease, ReleaseCatalog,
    Repository, Variant, VendorArtifact,
};
pub use crypto::{decode_tauri_public_key, verify_tauri_signature, TrustedPublicKey};
pub use loader::{
    MetadataLoader, MetadataTransport, TransportError, VerifiedDownload, VerifiedProduct,
    CATALOG_LIMIT, PRODUCT_LIMIT, REQUEST_TIMEOUT,
};

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("release updates are not configured")]
    NotConfigured,
    #[error("invalid release metadata: {0}")]
    InvalidMetadata(String),
    #[error("release URL is not allowed: {0}")]
    DisallowedUrl(String),
    #[error("release signature is invalid: {0}")]
    InvalidSignature(String),
    #[error("release metadata request failed: {0}")]
    Transport(String),
    #[error("release metadata exceeds the {limit} byte limit")]
    SizeLimit { limit: usize },
    #[error("release metadata is not valid UTF-8 or JSON: {0}")]
    Decode(String),
    #[error("no release has been published for {0}")]
    ProductUnavailable(Product),
    #[error("the requested package is not present in the signed release")]
    PackageUnavailable,
    #[error("download verification failed: {0}")]
    DownloadVerification(String),
    #[error("unsafe archive entry: {0}")]
    UnsafeArchiveEntry(String),
    #[error("file operation failed: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
