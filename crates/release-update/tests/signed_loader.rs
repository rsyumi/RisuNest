use std::collections::HashMap;
use std::io::Cursor;
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use minisign::{sign, KeyPair};
use risunest_release_update::{
    MetadataLoader, MetadataTransport, Product, Repository, TransportError, CATALOG_LIMIT,
};
use sha2::{Digest, Sha256};
use url::Url;

#[derive(Default)]
struct FixtureTransport(Mutex<HashMap<String, Vec<u8>>>);

#[async_trait]
impl MetadataTransport for FixtureTransport {
    async fn get(
        &self,
        url: &Url,
        max_bytes: usize,
        _: Duration,
    ) -> Result<Vec<u8>, TransportError> {
        let value = self
            .0
            .lock()
            .unwrap()
            .get(url.as_str())
            .cloned()
            .ok_or_else(|| TransportError::new(format!("missing {}", url)))?;
        if value.len() > max_bytes {
            return Err(TransportError::new("too large"));
        }
        Ok(value)
    }
}

fn node_catalog_and_product() -> (Vec<u8>, Vec<u8>) {
    let fixture = format!(
        "{}/../../tests/release/fixtures.mjs",
        env!("CARGO_MANIFEST_DIR").replace('\\', "/")
    );
    let script = format!(
        "import {{ productFixture, entryFixture }} from 'file:///{fixture}'; const release=productFixture('app'); const entry=entryFixture('app'); process.stdout.write(JSON.stringify(release)+'\\n'+JSON.stringify({{schema:'risunest.release-catalog/v1',publishedAt:'2026-09-15T00:00:00Z',publicationTag:release.tag,products:{{app:entry,sync:null}}}}));"
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
    (
        output.stdout[..split].to_vec(),
        output.stdout[split + 1..].to_vec(),
    )
}

fn node_json(expression: &str) -> Vec<u8> {
    let fixture = format!(
        "{}/../../tests/release/fixtures.mjs",
        env!("CARGO_MANIFEST_DIR").replace('\\', "/")
    );
    let script = format!(
        "import {{ productFixture }} from 'file:///{fixture}'; process.stdout.write(JSON.stringify({expression}));"
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
    output.stdout
}

fn signature(bytes: &[u8], pair: &KeyPair) -> Vec<u8> {
    STANDARD
        .encode(
            sign(Some(&pair.pk), &pair.sk, Cursor::new(bytes), None, None)
                .unwrap()
                .to_string(),
        )
        .into_bytes()
}

#[tokio::test]
async fn loads_latest_only_after_fixed_catalog_and_product_signatures_match() {
    let repository = Repository::risunest();
    let (product, mut catalog) = node_catalog_and_product();
    let product_hash = hex::encode(Sha256::digest(&product));
    let mut catalog_value: serde_json::Value = serde_json::from_slice(&catalog).unwrap();
    catalog_value["products"]["app"]["manifestSha256"] = product_hash.into();
    catalog = serde_json::to_vec(&catalog_value).unwrap();
    let pair = KeyPair::generate_unencrypted_keypair().unwrap();
    let key = STANDARD.encode(pair.pk.to_box().unwrap().into_string());
    let tag = "app-v1.0.0";
    let transport = FixtureTransport::default();
    let latest = repository.latest_catalog_url();
    let catalog_url = repository.release_asset_url(tag, "manifest.json").unwrap();
    let catalog_sig = repository
        .release_asset_url(tag, "manifest.json.sig")
        .unwrap();
    let product_url = repository
        .release_asset_url(tag, "product-manifest.json")
        .unwrap();
    let product_sig = repository
        .release_asset_url(tag, "product-manifest.json.sig")
        .unwrap();
    transport.0.lock().unwrap().extend([
        (latest.to_string(), catalog.clone()),
        (catalog_url.to_string(), catalog.clone()),
        (catalog_sig.to_string(), signature(&catalog, &pair)),
        (product_url.to_string(), product.clone()),
        (product_sig.to_string(), signature(&product, &pair)),
    ]);
    let verified = MetadataLoader::new(repository, &key, transport)
        .unwrap()
        .load_latest(Product::App)
        .await
        .unwrap();
    assert_eq!(verified.release.version, "1.0.0");

    let package = b"synthetic release bytes";
    let package_signature = String::from_utf8(signature(package, &pair)).unwrap();
    let path = std::env::temp_dir().join(format!(
        "risunest-verified-package-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&path, package).unwrap();
    verified
        .verify_signed_download_file(&verified.release.downloads[0], &path, &package_signature)
        .unwrap();
    std::fs::write(&path, b"changed").unwrap();
    assert!(verified
        .verify_signed_download_file(&verified.release.downloads[0], &path, &package_signature)
        .is_err());
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn rejects_a_cross_release_catalog_signature() {
    let repository = Repository::risunest();
    let (_, catalog) = node_catalog_and_product();
    let pair = KeyPair::generate_unencrypted_keypair().unwrap();
    let other = KeyPair::generate_unencrypted_keypair().unwrap();
    let key = STANDARD.encode(pair.pk.to_box().unwrap().into_string());
    let transport = FixtureTransport::default();
    transport.0.lock().unwrap().extend([
        (repository.latest_catalog_url().to_string(), catalog.clone()),
        (
            repository
                .release_asset_url("app-v1.0.0", "manifest.json")
                .unwrap()
                .to_string(),
            catalog.clone(),
        ),
        (
            repository
                .release_asset_url("app-v1.0.0", "manifest.json.sig")
                .unwrap()
                .to_string(),
            signature(&catalog, &other),
        ),
    ]);
    let error = MetadataLoader::new(repository, &key, transport)
        .unwrap()
        .load_latest(Product::App)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("signature"));
}

#[tokio::test]
async fn rejects_a_catalog_changed_after_latest_discovery() {
    let repository = Repository::risunest();
    let (_, discovery) = node_catalog_and_product();
    let mut fixed_value: serde_json::Value = serde_json::from_slice(&discovery).unwrap();
    fixed_value["publicationTag"] = "app-v1.0.1".into();
    let fixed = serde_json::to_vec(&fixed_value).unwrap();
    let pair = KeyPair::generate_unencrypted_keypair().unwrap();
    let key = STANDARD.encode(pair.pk.to_box().unwrap().into_string());
    let transport = FixtureTransport::default();
    transport.0.lock().unwrap().extend([
        (repository.latest_catalog_url().to_string(), discovery),
        (
            repository
                .release_asset_url("app-v1.0.0", "manifest.json")
                .unwrap()
                .to_string(),
            fixed.clone(),
        ),
        (
            repository
                .release_asset_url("app-v1.0.0", "manifest.json.sig")
                .unwrap()
                .to_string(),
            signature(&fixed, &pair),
        ),
    ]);
    assert!(MetadataLoader::new(repository, &key, transport)
        .unwrap()
        .load_latest(Product::App)
        .await
        .is_err());
}

#[tokio::test]
async fn rejects_changed_or_different_product_manifest_bytes() {
    for product in [
        {
            let mut value = node_json("productFixture('app')");
            value.push(b' ');
            value
        },
        node_json("productFixture('app', '1.0.1')"),
        node_json("productFixture('sync')"),
    ] {
        let repository = Repository::risunest();
        let (_, mut catalog) = node_catalog_and_product();
        let mut catalog_value: serde_json::Value = serde_json::from_slice(&catalog).unwrap();
        if !product.ends_with(b" ") {
            catalog_value["products"]["app"]["manifestSha256"] =
                hex::encode(Sha256::digest(&product)).into();
        }
        catalog = serde_json::to_vec(&catalog_value).unwrap();
        let pair = KeyPair::generate_unencrypted_keypair().unwrap();
        let key = STANDARD.encode(pair.pk.to_box().unwrap().into_string());
        let transport = FixtureTransport::default();
        transport.0.lock().unwrap().extend([
            (repository.latest_catalog_url().to_string(), catalog.clone()),
            (
                repository
                    .release_asset_url("app-v1.0.0", "manifest.json")
                    .unwrap()
                    .to_string(),
                catalog.clone(),
            ),
            (
                repository
                    .release_asset_url("app-v1.0.0", "manifest.json.sig")
                    .unwrap()
                    .to_string(),
                signature(&catalog, &pair),
            ),
            (
                repository
                    .release_asset_url("app-v1.0.0", "product-manifest.json")
                    .unwrap()
                    .to_string(),
                product.clone(),
            ),
            (
                repository
                    .release_asset_url("app-v1.0.0", "product-manifest.json.sig")
                    .unwrap()
                    .to_string(),
                signature(&product, &pair),
            ),
        ]);
        assert!(MetadataLoader::new(repository, &key, transport)
            .unwrap()
            .load_latest(Product::App)
            .await
            .is_err());
    }
}

#[tokio::test]
async fn bounds_the_unsigned_latest_discovery_response() {
    let repository = Repository::risunest();
    let pair = KeyPair::generate_unencrypted_keypair().unwrap();
    let key = STANDARD.encode(pair.pk.to_box().unwrap().into_string());
    let transport = FixtureTransport::default();
    transport.0.lock().unwrap().insert(
        repository.latest_catalog_url().to_string(),
        vec![b' '; CATALOG_LIMIT + 1],
    );
    let error = MetadataLoader::new(repository, &key, transport)
        .unwrap()
        .load_latest(Product::App)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("too large"));
}
