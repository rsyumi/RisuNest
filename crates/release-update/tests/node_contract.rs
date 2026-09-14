use std::process::Command;

use risunest_release_update::{ProductRelease, ReleaseCatalog, Repository};

fn node_fixture(expression: &str) -> Vec<u8> {
    let fixture = format!(
        "{}/../../tests/release/fixtures.mjs",
        env!("CARGO_MANIFEST_DIR").replace('\\', "/")
    );
    let script = format!(
        "import {{ productFixture, entryFixture, catalogFixture }} from 'file:///{fixture}'; process.stdout.write(JSON.stringify({expression}));"
    );
    let output = Command::new("node")
        .args(["--input-type=module", "--eval", &script])
        .output()
        .expect("Node is required by the repository test suite");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
fn rust_accepts_the_node_app14_and_sync16_contract_fixtures() {
    let repository = Repository::risunest();
    let app = ProductRelease::parse(&node_fixture("productFixture('app')"), &repository).unwrap();
    let sync = ProductRelease::parse(&node_fixture("productFixture('sync')"), &repository).unwrap();
    let catalog = ReleaseCatalog::parse(&node_fixture("catalogFixture()"), &repository).unwrap();
    assert_eq!(app.downloads.len(), 14);
    assert_eq!(sync.downloads.len(), 16);
    assert!(catalog.products.app.is_some() && catalog.products.sync.is_some());
}

#[test]
fn catalog_requires_both_explicit_slots_and_the_publication_product() {
    let repository = Repository::risunest();
    let mut value: serde_json::Value =
        serde_json::from_slice(&node_fixture("catalogFixture()")).unwrap();
    value["products"].as_object_mut().unwrap().remove("sync");
    value["publicationTag"] = "app-v1.0.0".into();
    assert!(ReleaseCatalog::parse(&serde_json::to_vec(&value).unwrap(), &repository).is_err());

    let mut value: serde_json::Value =
        serde_json::from_slice(&node_fixture("catalogFixture()")).unwrap();
    value["products"]["sync"] = serde_json::Value::Null;
    assert!(ReleaseCatalog::parse(&serde_json::to_vec(&value).unwrap(), &repository).is_err());
}

#[test]
fn platform_target_cannot_point_at_the_other_architecture() {
    let repository = Repository::risunest();
    let mut value: serde_json::Value =
        serde_json::from_slice(&node_fixture("productFixture('app')")).unwrap();
    value["platforms"]["windows-x86_64-nsis"] = value["platforms"]["windows-aarch64-nsis"].clone();
    assert!(ProductRelease::parse(&serde_json::to_vec(&value).unwrap(), &repository).is_err());
}

#[test]
fn compatibility_must_be_explicit_even_when_the_app_value_is_null() {
    let repository = Repository::risunest();
    let mut value: serde_json::Value =
        serde_json::from_slice(&node_fixture("productFixture('app')")).unwrap();
    value.as_object_mut().unwrap().remove("compatibility");
    assert!(ProductRelease::parse(&serde_json::to_vec(&value).unwrap(), &repository).is_err());
}

#[test]
fn prerelease_products_are_valid_but_stable_catalogs_reject_them() {
    let repository = Repository::risunest();
    ProductRelease::parse(
        &node_fixture("productFixture('app', '1.0.0-beta.1')"),
        &repository,
    )
    .unwrap();

    let mut value: serde_json::Value =
        serde_json::from_slice(&node_fixture("catalogFixture()")).unwrap();
    value["products"]["app"] =
        serde_json::from_slice(&node_fixture("entryFixture('app', '1.0.0-beta.1')")).unwrap();
    value["publicationTag"] = "app-v1.0.0-beta.1".into();
    assert!(ReleaseCatalog::parse(&serde_json::to_vec(&value).unwrap(), &repository).is_err());
}
