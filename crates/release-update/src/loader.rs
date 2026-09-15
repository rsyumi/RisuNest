use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;

use crate::{
    Download, Error, Product, ProductRelease, ReleaseCatalog, Repository, Result, TrustedPublicKey,
};

pub const CATALOG_LIMIT: usize = 1024 * 1024;
pub const PRODUCT_LIMIT: usize = 512 * 1024;
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, thiserror::Error)]
#[error("{message}")]
pub struct TransportError {
    message: String,
}

impl TransportError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[async_trait]
pub trait MetadataTransport: Send + Sync {
    async fn get(
        &self,
        url: &Url,
        max_bytes: usize,
        timeout: Duration,
    ) -> std::result::Result<Vec<u8>, TransportError>;
}

#[derive(Clone)]
pub struct MetadataLoader<T> {
    repository: Repository,
    public_key: TrustedPublicKey,
    transport: T,
}

impl<T: MetadataTransport> MetadataLoader<T> {
    pub fn new(repository: Repository, encoded_public_key: &str, transport: T) -> Result<Self> {
        Ok(Self {
            repository,
            public_key: TrustedPublicKey::from_tauri(encoded_public_key)?,
            transport,
        })
    }

    pub async fn load_latest(&self, product: Product) -> Result<VerifiedProduct> {
        let latest_url = self.repository.latest_catalog_url();
        let discovery = self.fetch(&latest_url, CATALOG_LIMIT).await?;
        let discovery: CatalogDiscovery =
            serde_json::from_slice(&discovery).map_err(|error| Error::Decode(error.to_string()))?;
        if discovery.schema != "risunest.release-catalog/v1" {
            return Err(Error::InvalidMetadata(
                "catalog discovery schema".to_owned(),
            ));
        }

        let catalog_url = self
            .repository
            .release_asset_url(&discovery.publication_tag, "manifest.json")?;
        let catalog_signature_url = self
            .repository
            .release_asset_url(&discovery.publication_tag, "manifest.json.sig")?;
        let (catalog_bytes, catalog_signature) = futures_pair(
            self.fetch(&catalog_url, CATALOG_LIMIT),
            self.fetch(&catalog_signature_url, PRODUCT_LIMIT),
        )
        .await?;
        let catalog_signature = signature_text(&catalog_signature)?;
        self.public_key.verify(&catalog_bytes, catalog_signature)?;
        let catalog = ReleaseCatalog::parse(&catalog_bytes, &self.repository)?;
        if catalog.publication_tag != discovery.publication_tag {
            return Err(Error::InvalidMetadata(
                "catalog publicationTag changed after discovery".to_owned(),
            ));
        }
        let entry = catalog.product(product)?.clone();
        let manifest_url = self
            .repository
            .validate_asset_url(&entry.manifest_url, &entry.release.tag)?;
        let manifest_signature_url = self
            .repository
            .release_asset_url(&entry.release.tag, "product-manifest.json.sig")?;
        let (manifest_bytes, manifest_signature) = futures_pair(
            self.fetch(&manifest_url, PRODUCT_LIMIT),
            self.fetch(&manifest_signature_url, PRODUCT_LIMIT),
        )
        .await?;
        let manifest_signature = signature_text(&manifest_signature)?;
        self.public_key
            .verify(&manifest_bytes, manifest_signature)?;
        verify_hash(&manifest_bytes, &entry.manifest_sha256)?;
        let release = ProductRelease::parse(&manifest_bytes, &self.repository)?;
        if release != entry.release || release.product != product {
            return Err(Error::InvalidMetadata(
                "catalog and product manifest snapshots differ".to_owned(),
            ));
        }

        Ok(VerifiedProduct {
            catalog,
            release,
            catalog_bytes,
            product_manifest_bytes: manifest_bytes,
            public_key: self.public_key.clone(),
        })
    }

    async fn fetch(&self, url: &Url, limit: usize) -> Result<Vec<u8>> {
        let bytes = self
            .transport
            .get(url, limit, REQUEST_TIMEOUT)
            .await
            .map_err(|error| Error::Transport(error.to_string()))?;
        if bytes.len() > limit {
            return Err(Error::SizeLimit { limit });
        }
        Ok(bytes)
    }
}

#[derive(Clone, Debug)]
pub struct VerifiedProduct {
    pub catalog: ReleaseCatalog,
    pub release: ProductRelease,
    pub catalog_bytes: Vec<u8>,
    pub product_manifest_bytes: Vec<u8>,
    public_key: TrustedPublicKey,
}

impl VerifiedProduct {
    pub fn select_download(&self, request: &crate::PackageRequest) -> Result<&Download> {
        if request.product != self.release.product {
            return Err(Error::PackageUnavailable);
        }
        self.release.select_download(request)
    }

    pub fn verify_download_bytes(
        &self,
        download: &Download,
        bytes: &[u8],
    ) -> Result<VerifiedDownload> {
        self.ensure_download_belongs(download)?;
        if bytes.len() as u64 != download.size {
            return Err(Error::DownloadVerification("size mismatch".to_owned()));
        }
        verify_hash(bytes, &download.sha256)?;
        Ok(VerifiedDownload {
            download: download.clone(),
            path: None,
        })
    }

    pub fn verify_download_file(
        &self,
        download: &Download,
        path: impl AsRef<Path>,
    ) -> Result<VerifiedDownload> {
        self.ensure_download_belongs(download)?;
        let path = path.as_ref();
        let file = File::open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() != download.size {
            return Err(Error::DownloadVerification("size mismatch".to_owned()));
        }
        let mut reader = BufReader::new(file);
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        let actual = hex::encode(hasher.finalize());
        if actual != download.sha256 {
            return Err(Error::DownloadVerification("SHA-256 mismatch".to_owned()));
        }
        Ok(VerifiedDownload {
            download: download.clone(),
            path: Some(path.to_path_buf()),
        })
    }

    pub fn verify_signed_download_bytes(
        &self,
        download: &Download,
        bytes: &[u8],
        encoded_signature: &str,
    ) -> Result<VerifiedDownload> {
        self.public_key.verify(bytes, encoded_signature)?;
        self.verify_download_bytes(download, bytes)
    }

    pub fn verify_signed_download_file(
        &self,
        download: &Download,
        path: impl AsRef<Path>,
        encoded_signature: &str,
    ) -> Result<VerifiedDownload> {
        let path = path.as_ref();
        self.public_key.verify_file(path, encoded_signature)?;
        self.verify_download_file(download, path)
    }

    fn ensure_download_belongs(&self, download: &Download) -> Result<()> {
        if !self
            .release
            .downloads
            .iter()
            .any(|candidate| candidate == download)
        {
            return Err(Error::DownloadVerification(
                "package is not part of this verified release".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct VerifiedDownload {
    pub download: Download,
    pub path: Option<PathBuf>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogDiscovery {
    schema: String,
    publication_tag: String,
}

fn signature_text(bytes: &[u8]) -> Result<&str> {
    std::str::from_utf8(bytes).map_err(|error| Error::Decode(error.to_string()))
}

fn verify_hash(bytes: &[u8], expected: &str) -> Result<()> {
    let actual = hex::encode(Sha256::digest(bytes));
    if actual != expected {
        return Err(Error::DownloadVerification("SHA-256 mismatch".to_owned()));
    }
    Ok(())
}

async fn futures_pair<A, B>(left: A, right: B) -> Result<(Vec<u8>, Vec<u8>)>
where
    A: std::future::Future<Output = Result<Vec<u8>>>,
    B: std::future::Future<Output = Result<Vec<u8>>>,
{
    // Keep the transport trait runtime-agnostic. Product consumers may choose
    // their own executor; requests remain independently bounded by the trait.
    let left = left.await?;
    let right = right.await?;
    Ok((left, right))
}
