//! The vault boundary. A snapshot copies whole database files and a capture
//! projects their rows, so the check is on the produced bytes rather than on
//! the absence of a table reference.
use super::*;
use crate::external_storage::{auth::SecretBytes, secrets::account_credential_slot};

const TOKEN: &str = "synthetic-account-token-4f1c9a27";

struct Never;

impl crate::local_backup::CancellationProbe for Never {
    fn is_cancelled(&self) -> bool {
        false
    }
}

fn files(path: &Path, found: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(path).expect("read repository directory") {
        let entry = entry.expect("read repository entry");
        let path = entry.path();
        if path.is_dir() {
            files(&path, found);
        } else {
            found.push(path);
        }
    }
}

fn contains_token(path: &Path) -> bool {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .expect("open stored file")
        .read_to_end(&mut bytes)
        .expect("read stored file");
    bytes
        .windows(TOKEN.len())
        .any(|part| part == TOKEN.as_bytes())
}

#[cfg(windows)]
#[test]
fn the_account_token_never_reaches_a_snapshot_an_export_or_a_sync_capture() {
    let (directory, mut store, _) = open_fixture();
    let root = directory.path().to_owned();
    let slot = account_credential_slot(&root);
    let credential = format!(r#"{{"id":"account-1","token":"{TOKEN}","data":{{}}}}"#);
    slot.write(&SecretBytes(zeroize::Zeroizing::new(
        credential.clone().into_bytes(),
    )))
    .expect("store the account token");

    // Everything the account flow keeps beside the token stays in the library
    // or in the device file, so both carry realistic content here.
    let device = store.device_store().expect("open device store");
    device
        .write_setting(
            "official-account.association.v1",
            &json!({ "officialAssociation:account-1": "{\"revision\":1}" }),
        )
        .expect("store the association marker");
    device
        .write_setting(
            "official-account.asset-ledger.v1",
            &json!({ "officialPublishedAssets:account-1": "{\"version\":1}" }),
        )
        .expect("store the asset ledger");

    let generation = active_generation(&store.connection).expect("read active generation");
    let authority = json!({
        "format": "v2",
        "migrationId": "synthetic-vault-boundary",
        "compatibilityHash": "a".repeat(64),
    })
    .to_string();
    store
        .connection
        .execute(
            "UPDATE asset_repository_authority SET value=?2 WHERE generation=?1",
            params![generation, authority],
        )
        .expect("stamp the capture authority");

    let snapshot = store
        .snapshot_create("vault-boundary")
        .expect("create a snapshot");
    assert!(!snapshot.id.is_empty());

    let revision = store.revision().expect("read the current revision");
    let lease = store
        .acquire_revision(revision)
        .expect("acquire the export revision");
    let exported = store
        .export_risu_save(&lease.lease, false)
        .expect("export a RisuSave");
    assert!(exported.bytes > 0);

    let hydration = store
        .hydrate_external_capture_dependencies("vault-boundary", &Never)
        .expect("hydrate the capture dependencies");
    let captured = store
        .capture_external_library("vault-boundary", &hydration, &Never)
        .expect("capture the library");
    assert!(captured.projected_records > 0);

    let vault_file = root.join("account-credentials").join("official-account");
    assert!(vault_file.is_file(), "the vault slot must hold the token");
    assert!(
        !contains_token(&vault_file),
        "the vault must not keep the token in the clear"
    );
    assert_eq!(
        slot.read().expect("read the stored token").0.as_slice(),
        credential.as_bytes()
    );

    let mut stored = Vec::new();
    files(&root, &mut stored);
    let leaked: Vec<_> = stored
        .iter()
        .filter(|path| !path.starts_with(root.join("account-credentials")))
        .filter(|path| contains_token(path))
        .collect();
    assert!(
        leaked.is_empty(),
        "the account token reached stored files: {leaked:?}"
    );
    assert!(
        stored
            .iter()
            .any(|path| path.extension().is_some_and(|extension| extension == "sqlite")),
        "the scan must have covered the stored databases"
    );
}
