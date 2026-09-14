use async_trait::async_trait;
use risunest_release_update::{
    compare_versions, Architecture, MetadataLoader, MetadataTransport, OperatingSystem,
    PackageFormat, PackageRequest, Product, Repository, TransportError, Variant,
};
use serde::Serialize;
use std::{cmp::Ordering, time::Duration};

#[derive(Clone)]
struct HttpTransport {
    client: reqwest::Client,
}

#[async_trait]
impl MetadataTransport for HttpTransport {
    async fn get(
        &self,
        url: &reqwest::Url,
        max_bytes: usize,
        timeout: Duration,
    ) -> Result<Vec<u8>, TransportError> {
        let mut response = self
            .client
            .get(url.clone())
            .timeout(timeout)
            .send()
            .await
            .map_err(|_| TransportError::new("request failed"))?;
        if !response.status().is_success() {
            return Err(TransportError::new("unexpected HTTP status"));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| TransportError::new("response incomplete"))?
        {
            if bytes.len().saturating_add(chunk.len()) > max_bytes {
                return Err(TransportError::new("response exceeds limit"));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

#[derive(Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UpdateCheck {
    status: &'static str,
    current_version: String,
    available_version: String,
    download_url: String,
}

pub(crate) async fn check() -> Result<UpdateCheck, &'static str> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                return attempt.error("too many redirects");
            }
            let url = attempt.url();
            let host = url.host_str().unwrap_or_default();
            if url.scheme() == "https"
                && url.port().is_none()
                && (host == "github.com" || host.ends_with(".githubusercontent.com"))
            {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|_| "update-network-unavailable")?;
    check_with_transport(
        env!("CARGO_PKG_VERSION"),
        option_env!("RISUNEST_UPDATE_PUBLIC_KEY").unwrap_or(""),
        HttpTransport { client },
    )
    .await
}

async fn check_with_transport<T: MetadataTransport>(
    current_version: &str,
    public_key: &str,
    transport: T,
) -> Result<UpdateCheck, &'static str> {
    let loader = MetadataLoader::new(Repository::risunest(), public_key, transport)
        .map_err(release_error)?;
    let product = loader
        .load_latest(Product::Sync)
        .await
        .map_err(release_error)?;
    let request = package_request()?;
    let download = product.select_download(&request).map_err(release_error)?;
    let ordering = compare_versions(&product.release.version, current_version)
        .map_err(|_| "installed-version-invalid")?;
    Ok(UpdateCheck {
        status: if ordering == Ordering::Greater {
            "available"
        } else {
            "current"
        },
        current_version: current_version.to_owned(),
        available_version: product.release.version.clone(),
        download_url: download.url.clone(),
    })
}

fn package_request() -> Result<PackageRequest, &'static str> {
    let os = if cfg!(windows) {
        OperatingSystem::Windows
    } else if cfg!(target_os = "macos") {
        OperatingSystem::Darwin
    } else if cfg!(target_os = "linux") {
        OperatingSystem::Linux
    } else {
        return Err("update-platform-unsupported");
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => Architecture::X86_64,
        "aarch64" => Architecture::Aarch64,
        _ => return Err("update-architecture-unsupported"),
    };
    Ok(PackageRequest {
        product: Product::Sync,
        variant: Variant::Raw,
        os,
        arch,
        format: if cfg!(windows) {
            PackageFormat::Zip
        } else {
            PackageFormat::TarGz
        },
    })
}

fn release_error(error: risunest_release_update::Error) -> &'static str {
    match error {
        risunest_release_update::Error::NotConfigured => "update-not-configured",
        risunest_release_update::Error::InvalidSignature(_) => "update-signature-invalid",
        risunest_release_update::Error::Transport(_) => "update-network-unavailable",
        risunest_release_update::Error::PackageUnavailable => "update-package-unavailable",
        _ => "update-metadata-invalid",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use minisign::{sign, KeyPair};
    use std::{collections::HashMap, io::Cursor, process::Command, sync::Mutex};

    #[derive(Default)]
    struct FixtureTransport(Mutex<HashMap<String, Vec<u8>>>);

    #[async_trait]
    impl MetadataTransport for FixtureTransport {
        async fn get(
            &self,
            url: &reqwest::Url,
            max_bytes: usize,
            _: Duration,
        ) -> Result<Vec<u8>, TransportError> {
            let value = self
                .0
                .lock()
                .unwrap()
                .get(url.as_str())
                .cloned()
                .ok_or_else(|| TransportError::new("synthetic network failure"))?;
            if value.len() > max_bytes {
                return Err(TransportError::new("synthetic response too large"));
            }
            Ok(value)
        }
    }

    struct FailingTransport;

    #[async_trait]
    impl MetadataTransport for FailingTransport {
        async fn get(
            &self,
            _: &reqwest::Url,
            _: usize,
            _: Duration,
        ) -> Result<Vec<u8>, TransportError> {
            Err(TransportError::new("synthetic network failure"))
        }
    }

    fn fixture(version: &str, signer: &KeyPair) -> (String, FixtureTransport) {
        let fixture = format!(
            "{}/../../tests/release/fixtures.mjs",
            env!("CARGO_MANIFEST_DIR").replace('\\', "/")
        );
        let script = format!(
            "import {{ productFixture, entryFixture }} from 'file:///{fixture}'; const release=productFixture('sync','{version}'); const app=entryFixture('app'); const sync={{manifestUrl:release.downloads[0].url.replace(/[^/]+$/, 'product-manifest.json'),manifestSha256:'',release}}; const product=Buffer.from(JSON.stringify(release)); const crypto=await import('node:crypto'); sync.manifestSha256=crypto.createHash('sha256').update(product).digest('hex'); const catalog={{schema:'risunest.release-catalog/v1',publishedAt:'2026-09-15T00:00:00Z',publicationTag:release.tag,products:{{app,sync}}}}; process.stdout.write(product.toString()+'\\n'+JSON.stringify(catalog));"
        );
        let output = Command::new("node")
            .args(["--input-type=module", "--eval", &script])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let split = output
            .stdout
            .iter()
            .position(|byte| *byte == b'\n')
            .unwrap();
        let product = output.stdout[..split].to_vec();
        let catalog = output.stdout[split + 1..].to_vec();
        let repository = Repository::risunest();
        let tag = format!("sync-v{version}");
        let transport = FixtureTransport::default();
        transport.0.lock().unwrap().extend([
            (repository.latest_catalog_url().to_string(), catalog.clone()),
            (
                repository
                    .release_asset_url(&tag, "manifest.json")
                    .unwrap()
                    .to_string(),
                catalog.clone(),
            ),
            (
                repository
                    .release_asset_url(&tag, "manifest.json.sig")
                    .unwrap()
                    .to_string(),
                signature(&catalog, signer),
            ),
            (
                repository
                    .release_asset_url(&tag, "product-manifest.json")
                    .unwrap()
                    .to_string(),
                product.clone(),
            ),
            (
                repository
                    .release_asset_url(&tag, "product-manifest.json.sig")
                    .unwrap()
                    .to_string(),
                signature(&product, signer),
            ),
        ]);
        (
            STANDARD.encode(signer.pk.to_box().unwrap().into_string()),
            transport,
        )
    }

    fn signature(bytes: &[u8], signer: &KeyPair) -> Vec<u8> {
        STANDARD
            .encode(
                sign(Some(&signer.pk), &signer.sk, Cursor::new(bytes), None, None)
                    .unwrap()
                    .to_string(),
            )
            .into_bytes()
    }

    #[tokio::test]
    async fn reports_current_and_available_raw_packages() {
        for (current, status) in [("1.0.0", "current"), ("0.1.0", "available")] {
            let signer = KeyPair::generate_unencrypted_keypair().unwrap();
            let (key, transport) = fixture("1.0.0", &signer);
            let result = check_with_transport(current, &key, transport)
                .await
                .unwrap();
            assert_eq!(result.status, status);
            assert_eq!(result.current_version, current);
            assert_eq!(result.available_version, "1.0.0");
            assert!(result.download_url.contains("/sync-raw-"));
            assert!(result.download_url.contains(std::env::consts::ARCH));
        }
    }

    #[tokio::test]
    async fn rejects_invalid_signed_metadata_without_returning_a_download() {
        let trusted = KeyPair::generate_unencrypted_keypair().unwrap();
        let untrusted = KeyPair::generate_unencrypted_keypair().unwrap();
        let (_, transport) = fixture("1.0.0", &untrusted);
        let key = STANDARD.encode(trusted.pk.to_box().unwrap().into_string());
        assert_eq!(
            check_with_transport("0.1.0", &key, transport)
                .await
                .unwrap_err(),
            "update-signature-invalid"
        );
    }

    #[tokio::test]
    async fn reports_network_failure_without_falling_back_to_an_unverified_url() {
        let signer = KeyPair::generate_unencrypted_keypair().unwrap();
        let key = STANDARD.encode(signer.pk.to_box().unwrap().into_string());
        assert_eq!(
            check_with_transport("0.1.0", &key, FailingTransport)
                .await
                .unwrap_err(),
            "update-network-unavailable"
        );
    }
}
