use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
    sync::Mutex,
};

static INDEX_LOCK: Mutex<()> = Mutex::new(());
const DIRECTORY: &str = "owned-secret-index";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Purpose {
    Provider,
    RepositoryKey,
    AccountCredential,
    ServerSync,
}

impl Purpose {
    fn name(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::RepositoryKey => "repository-key",
            Self::AccountCredential => "account-credential",
            Self::ServerSync => "server-sync",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Backend { System, PrivateFile }

#[derive(Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Entry {
    backend: Backend,
    purpose: Purpose,
    id: String,
}

pub(crate) fn account_id(root: &Path) -> String {
    use sha2::Digest;
    format!(
        "official-account-{}",
        hex::encode(sha2::Sha256::digest(root.as_os_str().as_encoded_bytes()))
    )
}

/// An account entry another data root entry path wrote, for example before a
/// link at the data root was pointed at this directory. It names that path's
/// OS vault item.
fn foreign_account(root: &Path, entry: &Entry) -> bool {
    entry.purpose == Purpose::AccountCredential
        && entry.id != account_id(root)
        && entry.id.strip_prefix("official-account-").is_some_and(|hash| {
            hash.len() == 64 && hash.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        })
}

fn validate(root: &Path, entry: &Entry) -> Result<(), String> {
    let valid = match entry.purpose {
        Purpose::AccountCredential => entry.id == account_id(root),
        _ => uuid::Uuid::parse_str(&entry.id)
            .is_ok_and(|id| id.get_version_num() == 4 && id.to_string() == entry.id),
    };
    if valid {
        Ok(())
    } else {
        Err("secret-index-corrupt".into())
    }
}

fn ensure_directory(root: &Path) -> Result<std::path::PathBuf, String> {
    let path = root.join(DIRECTORY);
    fs::create_dir_all(root).map_err(|_| "secret-index-unavailable")?;
    match fs::create_dir(&path) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(_) => return Err("secret-index-unavailable".into()),
    }
    let metadata = fs::symlink_metadata(&path).map_err(|_| "secret-index-unavailable")?;
    if !metadata.is_dir() || crate::trust_boundary::is_link_like(&metadata) {
        return Err("secret-index-corrupt".into());
    }
    #[cfg(unix)]
    fs::File::open(root)
        .and_then(|file| file.sync_all())
        .map_err(|_| "secret-index-unavailable")?;
    Ok(path)
}

fn read_entry(root: &Path, path: &Path) -> Result<Entry, String> {
    read_inventory_entry(root, path, false)
}

fn read_inventory_entry(root: &Path, path: &Path, foreign_accounts: bool) -> Result<Entry, String> {
    let name = path.file_name().and_then(|name| name.to_str()).ok_or("secret-index-corrupt")?;
    let (purpose, id) = name.strip_suffix(".json").and_then(|name| name.split_once("--"))
        .ok_or("secret-index-corrupt")?;
    let purpose = match purpose {
        "provider" => Purpose::Provider,
        "repository-key" => Purpose::RepositoryKey,
        "account-credential" => Purpose::AccountCredential,
        "server-sync" => Purpose::ServerSync,
        _ => return Err("secret-index-corrupt".into()),
    };
    let entry = Entry { purpose, id: id.to_owned(), backend: Backend::System };
    if !(foreign_accounts && foreign_account(root, &entry)) {
        validate(root, &entry)?;
    }
    let metadata = fs::symlink_metadata(path).map_err(|_| "secret-index-unavailable")?;
    if !metadata.is_file()
        || crate::trust_boundary::is_link_like(&metadata)
        || metadata.len() > 1024
    {
        return Err("secret-index-corrupt".into());
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|_| "secret-index-unavailable")?
        .take(1025)
        .read_to_end(&mut bytes)
        .map_err(|_| "secret-index-unavailable")?;
    let contents: Entry = serde_json::from_slice(&bytes).map_err(|_| "secret-index-corrupt")?;
    if contents.purpose != entry.purpose || contents.id != entry.id { return Err("secret-index-corrupt".into()); }
    Ok(contents)
}

fn entry_path(root: &Path, purpose: Purpose, id: &str) -> Result<std::path::PathBuf, String> {
    validate(root, &Entry { purpose, id: id.to_owned(), backend: Backend::System })?;
    let directory = root.join(DIRECTORY);
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if !metadata.is_dir() || crate::trust_boundary::is_link_like(&metadata) => return Err("secret-index-corrupt".into()),
        Ok(_) => (),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(_) => return Err("secret-index-unavailable".into()),
    }
    Ok(directory.join(format!("{}--{}.json", purpose.name(), id)))
}

fn existing(root: &Path, path: &Path) -> Result<Option<Entry>, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => read_entry(root, path).map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err("secret-index-unavailable".into()),
    }
}

pub(crate) fn backend(root: &Path, purpose: Purpose, id: &str) -> Result<Option<Backend>, String> {
    let _lock = INDEX_LOCK.lock().map_err(|_| "secret-index-unavailable")?;
    Ok(existing(root, &entry_path(root, purpose, id)?)?.map(|entry| entry.backend))
}

pub(crate) fn remove_one(root: &Path, purpose: Purpose, id: &str, remove: impl FnOnce(Backend) -> Result<(), String>) -> Result<(), String> {
    let _lock = INDEX_LOCK.lock().map_err(|_| "secret-index-unavailable")?;
    let path = entry_path(root, purpose, id)?;
    if let Some(entry) = existing(root, &path)? {
        remove(entry.backend)?;
        fs::remove_file(path).map_err(|_| "secret-index-unavailable")?;
    }
    Ok(())
}

// Persist the selected backend before creating the secret, including failed attempts.
pub(crate) fn routed_write<T>(
    root: &Path, purpose: Purpose, id: &str,
    select: impl FnOnce() -> Result<Backend, String>,
    write: impl FnOnce(Backend) -> Result<T, String>,
) -> Result<T, String> {
    let _lock = INDEX_LOCK.lock().map_err(|_| "secret-index-unavailable")?;
    let path = entry_path(root, purpose, id)?;
    if let Some(entry) = existing(root, &path)? { return write(entry.backend); }
    let entry = Entry { purpose, id: id.to_owned(), backend: select()? };
    let directory = ensure_directory(root)?;
    let mut file = tempfile::NamedTempFile::new_in(root).map_err(|_| "secret-index-unavailable")?;
    let bytes = serde_json::to_vec(&entry).map_err(|_| "secret-index-unavailable")?;
    file.write_all(&bytes).and_then(|_| file.as_file().sync_all()).map_err(|_| "secret-index-unavailable")?;
    file.persist_noclobber(&path).map_err(|_| "secret-index-unavailable")?;
    #[cfg(unix)]
    fs::File::open(&directory).and_then(|file| file.sync_all()).map_err(|_| "secret-index-unavailable")?;
    #[cfg(not(unix))]
    let _ = directory;
    write(entry.backend)
}

#[cfg(test)]
fn tracked_write<T>(root: &Path, purpose: Purpose, id: &str, write: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    routed_write(root, purpose, id, || Ok(Backend::System), |_| write())
}

fn remove_with(
    root: &Path,
    mut remove: impl FnMut(Purpose, &str, Backend) -> Result<(), String>,
) -> Result<(), String> {
    let _lock = INDEX_LOCK.lock().map_err(|_| "secret-index-unavailable")?;
    let directory = root.join(DIRECTORY);
    let metadata = match fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("secret-index-unavailable".into()),
    };
    if !metadata.is_dir() || crate::trust_boundary::is_link_like(&metadata) {
        return Err("secret-index-corrupt".into());
    }
    let mut entries = Vec::new();
    for file in fs::read_dir(&directory).map_err(|_| "secret-index-unavailable")? {
        let path = file.map_err(|_| "secret-index-unavailable")?.path();
        let entry = read_inventory_entry(root, &path, true)?;
        entries.push((path, entry));
    }
    // Validate the complete inventory before deleting any credential. A
    // foreign account entry leaves the vault item to the path that owns it.
    for (path, entry) in entries {
        if !foreign_account(root, &entry) {
            remove(entry.purpose, &entry.id, entry.backend)?;
        }
        fs::remove_file(path).map_err(|_| "secret-index-unavailable")?;
    }
    fs::remove_dir(directory).map_err(|_| "secret-index-unavailable".into())
}

pub(crate) fn remove_all(root: &Path) -> Result<(), String> {
    remove_with(root, |purpose, id, backend| crate::device_secrets::remove_owned(root, purpose, id, backend))?;
    #[cfg(target_os = "android")]
    crate::device_secrets::remove_android_keys()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_entries_cannot_guess_the_storage_backend() {
        let root = tempfile::tempdir().unwrap();
        let directory = ensure_directory(root.path()).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        fs::write(directory.join(format!("provider--{id}.json")), b"").unwrap();
        assert!(remove_with(root.path(), |_, _, _| panic!("unknown backend cannot be deleted")).is_err());
    }

    #[test]
    fn backend_is_durable_and_never_reselected_after_failed_creation() {
        for selected in [Backend::System, Backend::PrivateFile] {
            let root = tempfile::tempdir().unwrap();
            let id = uuid::Uuid::new_v4().to_string();
            assert!(routed_write(root.path(), Purpose::Provider, &id, || Ok(selected), |_| Err::<(), _>("failed".into())).is_err());
            assert_eq!(backend(root.path(), Purpose::Provider, &id).unwrap(), Some(selected));
            routed_write(root.path(), Purpose::Provider, &id, || panic!("must retain selected storage"), |actual| { assert_eq!(actual, selected); Ok(()) }).unwrap();
            assert!(remove_one(root.path(), Purpose::Provider, &id, |_| Err("locked".into())).is_err());
            assert_eq!(backend(root.path(), Purpose::Provider, &id).unwrap(), Some(selected));
            remove_one(root.path(), Purpose::Provider, &id, |actual| { assert_eq!(actual, selected); Ok(()) }).unwrap();
            assert_eq!(backend(root.path(), Purpose::Provider, &id).unwrap(), None);
        }
    }

    #[test]
    fn torn_or_mismatching_nonempty_entries_block_the_entire_inventory() {
        for body in [b"{".to_vec(), serde_json::to_vec(&Entry {
            purpose: Purpose::Provider, id: uuid::Uuid::new_v4().to_string(), backend: Backend::System,
        }).unwrap()] {
            let root = tempfile::tempdir().unwrap();
            let directory = ensure_directory(root.path()).unwrap();
            let id = uuid::Uuid::new_v4().to_string();
            fs::write(directory.join(format!("provider--{id}.json")), body).unwrap();
            assert_eq!(remove_with(root.path(), |_, _, _| panic!("must validate first")).unwrap_err(), "secret-index-corrupt");
            assert!(tracked_write(root.path(), Purpose::Provider, &id,
                || -> Result<(), String> { panic!("must not overwrite a mismatching index") }).is_err());
        }
    }

    #[test]
    fn ownership_is_complete_and_durable_before_the_secret_write() {
        let root = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        tracked_write(root.path(), Purpose::Provider, &id, || {
            let directory = root.path().join(DIRECTORY);
            let entries = fs::read_dir(&directory).unwrap().collect::<Vec<_>>();
            assert_eq!(entries.len(), 1);
            let entry = read_entry(root.path(), &entries[0].as_ref().unwrap().path()).unwrap();
            assert_eq!(entry, Entry { purpose: Purpose::Provider, id: id.clone(), backend: Backend::System });
            assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
            Ok(())
        }).unwrap();
    }

    #[test]
    fn failed_creation_is_tracked_and_locked_store_remains_retryable() {
        let root = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        assert!(
            tracked_write(root.path(), Purpose::Provider, &id, || Err::<(), _>(
                "locked".into()
            ))
            .is_err()
        );
        assert!(remove_with(root.path(), |purpose, found, _| {
            assert_eq!(purpose, Purpose::Provider);
            assert_eq!(found, id);
            Err("locked".into())
        })
        .is_err());
        let mut removed = 0;
        remove_with(root.path(), |_, _, _| {
            removed += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(removed, 1);
        remove_with(root.path(), |_, _, _| panic!("already removed")).unwrap();
    }

    #[test]
    fn corrupt_inventory_blocks_all_deletion() {
        let root = tempfile::tempdir().unwrap();
        tracked_write(
            root.path(),
            Purpose::ServerSync,
            &uuid::Uuid::new_v4().to_string(),
            || Ok(()),
        )
        .unwrap();
        fs::write(root.path().join(DIRECTORY).join("corrupt.json"), b"{}").unwrap();
        assert_eq!(
            remove_with(root.path(), |_, _, _| panic!("must validate first")).unwrap_err(),
            "secret-index-corrupt"
        );
    }

    #[test]
    fn invalid_or_unwritable_inventory_never_creates_a_secret() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            tracked_write(root.path(), Purpose::Provider, "../outside", || {
                panic!("invalid identity must never reach the OS store");
                #[allow(unreachable_code)]
                Ok(())
            })
            .is_err()
        );
        fs::write(root.path().join(DIRECTORY), b"blocked").unwrap();
        assert!(tracked_write(
            root.path(),
            Purpose::ServerSync,
            &uuid::Uuid::new_v4().to_string(),
            || {
                panic!("inventory must be durable before secret creation");
                #[allow(unreachable_code)]
                Ok(())
            }
        )
        .is_err());
    }

    #[test]
    fn cleanup_removes_every_exact_owned_identity_without_reading_secrets() {
        let root = tempfile::tempdir().unwrap();
        let mut expected = Vec::new();
        for purpose in [
            Purpose::Provider,
            Purpose::RepositoryKey,
            Purpose::ServerSync,
            Purpose::AccountCredential,
        ] {
            let id = if purpose == Purpose::AccountCredential {
                account_id(root.path())
            } else {
                uuid::Uuid::new_v4().to_string()
            };
            tracked_write(root.path(), purpose, &id, || Ok(())).unwrap();
            expected.push((purpose, id));
        }
        remove_with(root.path(), |purpose, id, _| {
            let position = expected
                .iter()
                .position(|entry| entry.0 == purpose && entry.1 == id)
                .unwrap();
            expected.remove(position);
            Ok(())
        })
        .unwrap();
        assert!(expected.is_empty());
    }

    #[test]
    fn account_slots_are_root_scoped_and_index_contains_only_identity() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        assert_ne!(account_id(first.path()), account_id(second.path()));
        let id = account_id(first.path());
        tracked_write(first.path(), Purpose::AccountCredential, &id, || Ok(())).unwrap();
        tracked_write(first.path(), Purpose::AccountCredential, &id, || Ok(())).unwrap();
        // A data root a link brought here from another entry path keeps that
        // path's entry, whose vault item belongs to the other path.
        let foreign = account_id(second.path());
        fs::write(
            first.path().join(DIRECTORY).join(format!("account-credential--{foreign}.json")),
            serde_json::to_vec(&Entry { backend: Backend::System, purpose: Purpose::AccountCredential, id: foreign.clone() }).unwrap(),
        )
        .unwrap();
        assert!(tracked_write(first.path(), Purpose::AccountCredential, &foreign, || -> Result<(), String> {
            panic!("another path's slot is never written here")
        })
        .is_err());
        let mut count = 0;
        remove_with(first.path(), |purpose, found, _| {
            assert_eq!(purpose, Purpose::AccountCredential);
            assert_eq!(found, id);
            count += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(count, 1);
        assert!(!first.path().join(DIRECTORY).exists());
    }
}
