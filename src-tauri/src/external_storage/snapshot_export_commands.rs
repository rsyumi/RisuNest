//! User-selected standalone archive export for authenticated history entries.
//! Remote bytes are fully downloaded and verified before the archive is built.
use super::{
    capabilities::Capabilities,
    connection_commands,
    contract::{Cancellation, ErrorKind, ProviderError, Result},
    control, leases, runtime, snapshot_export, snapshot_restore,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Write},
};
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_fs::{FilePath, FsExt, OpenOptions};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ExportSnapshotRequest {
    connection_id: String,
    snapshot_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExportSnapshotResponse {
    cancelled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    destination: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 1024 && !value.contains('\0')
}

fn archive_name(snapshot_id: &str) -> String {
    let safe: String = snapshot_id
        .chars()
        .filter(|value| value.is_ascii_alphanumeric() || matches!(value, '-' | '_'))
        .take(64)
        .collect();
    format!(
        "RisuNest-{}.risunest",
        if safe.is_empty() { "snapshot" } else { &safe }
    )
}

fn selected_path(app: &AppHandle, snapshot_id: &str) -> Option<FilePath> {
    app.dialog()
        .file()
        .add_filter("RisuNest backup", &["risunest"])
        .set_file_name(archive_name(snapshot_id))
        .blocking_save_file()
}

fn publish_uri_destination(
    app: &AppHandle,
    selected: FilePath,
    candidate: &std::path::Path,
    expected_hash: &str,
    cancel: &Cancellation,
) -> Result<()> {
    let mut source = File::open(candidate).map_err(|_| ProviderError::new(ErrorKind::Transient))?;
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    let mut output = app
        .fs()
        .open(selected.clone(), options)
        .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        cancel.check()?;
        let count = source
            .read(&mut buffer)
            .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
        if count == 0 {
            break;
        }
        output
            .write_all(&buffer[..count])
            .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
    }
    output
        .flush()
        .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
    output
        .sync_all()
        .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
    drop(output);
    let mut options = OpenOptions::new();
    options.read(true);
    let mut output = app
        .fs()
        .open(selected, options)
        .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
    let mut digest = Sha256::new();
    loop {
        cancel.check()?;
        let count = output
            .read(&mut buffer)
            .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    if hex::encode(digest.finalize()) != expected_hash {
        return Err(ProviderError::new(ErrorKind::Corrupt));
    }
    Ok(())
}

/// Announces this command in the repository, runs its remote work and hands
/// the announcement back. A command has no durable job to name it, so the
/// identity is drawn per call and lives only as long as the call does. Every
/// remote request of `body` is awaited before this returns, which is what makes
/// the release point the one where nothing of this command is still in flight;
/// an end this device cannot see, such as a panic, keeps the lease instead. A
/// repository that cannot remove anything has nothing to announce and nothing
/// to wait for.
async fn with_export_lease<T>(
    context: &leases::LeaseContext<'_>,
    capabilities: &Capabilities,
    now_ms: u64,
    cancel: &Cancellation,
    body: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    if capabilities.require_cleanup().is_err() {
        return body.await;
    }
    let lease_id = uuid::Uuid::new_v4().to_string();
    let outcome = match leases::admit(context, &lease_id, now_ms, cancel).await {
        Ok(_) => body.await,
        Err(error) => Err(error),
    };
    if let Err(error) = leases::release(context, &lease_id).await {
        crate::nlog!(
            "error",
            "External storage lease could not be released: {error}"
        );
    }
    outcome
}

#[tauri::command(async)]
pub(crate) async fn external_storage_export_snapshot(
    app: AppHandle,
    request: ExportSnapshotRequest,
) -> Result<ExportSnapshotResponse> {
    if !valid_id(&request.connection_id) || !valid_id(&request.snapshot_id) {
        return Err(ProviderError::new(ErrorKind::Corrupt));
    }
    let Some(selected) = selected_path(&app, &request.snapshot_id) else {
        return Ok(ExportSnapshotResponse {
            cancelled: true,
            destination: None,
            sha256: None,
        });
    };
    let root = runtime::root(&app)?;
    std::fs::create_dir_all(root.join("external-storage"))
        .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
    let staging = tempfile::Builder::new()
        .prefix("external-snapshot-download-")
        .tempdir_in(root.join("external-storage"))
        .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
    let cancel = Cancellation::default();
    let connected = connection_commands::open_connected(&app, &request.connection_id).await?;
    let writer_id = crate::persistent_store::commands::with_store_mut(app.state(), |store| {
        store.external_identity()
    })
    .map_err(runtime::local_error)?
    .store_id;
    let context = leases::LeaseContext {
        root: root.as_path(),
        connection_id: &connected.stored.id,
        writer_id: &writer_id,
        descriptor: &connected.stored.descriptor,
        root_key: &connected.root_key,
        provider: connected.provider.as_ref(),
        repository: &connected.handle,
    };
    let prepared = with_export_lease(
        &context,
        &connected.stored.capabilities,
        runtime::now_ms(),
        &cancel,
        async {
            let remote = control::find_snapshot(&connected, &request.snapshot_id, &cancel).await?;
            let prepared = snapshot_restore::download_snapshot(
                &remote,
                &staging.path().join("verified"),
                &connected.root_key,
                connected.provider.as_ref(),
                &connected.handle,
                &cancel,
            )
            .await?;
            if prepared.snapshot_id != request.snapshot_id {
                return Err(ProviderError::new(ErrorKind::Corrupt));
            }
            Ok(prepared)
        },
    )
    .await?;
    let path_destination = selected.clone().into_path().ok();
    let local_destination = path_destination
        .clone()
        .unwrap_or_else(|| staging.path().join("selected-snapshot.risunest"));
    let receipt = snapshot_export::export_verified_snapshot(
        prepared,
        &local_destination,
        &staging.path().join("export"),
        &cancel,
    )?;
    if path_destination.is_none() {
        publish_uri_destination(
            &app,
            selected.clone(),
            &local_destination,
            &receipt.sha256,
            &cancel,
        )?;
    }
    Ok(ExportSnapshotResponse {
        cancelled: false,
        destination: Some(selected.to_string()),
        sha256: Some(receipt.sha256),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_storage::{
        capabilities::Evidence,
        contract::{lease_object_id, LeaseKind, ObjectRole, RepositoryHandle},
        fake::{self, FakeProvider},
        gc_store::{GcStore, LeaseIntent, LeaseState},
    };
    use risunest_external_storage_format::format::{Descriptor, Strategy};
    use std::{
        cell::RefCell,
        path::PathBuf,
        sync::atomic::{AtomicBool, Ordering},
    };

    const DAY: u64 = 24 * 60 * 60 * 1000;
    const NOW: u64 = 1_000 * DAY;
    const CONNECTION: &str = "connection";

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    /// Everything a cleanup needs evidence of, on top of what the shared fake
    /// already states. Without it a repository never announces anything.
    fn cleanup_ready() -> Capabilities {
        Capabilities {
            delete_objects: Evidence::Synthetic,
            snapshot_discovery: Evidence::Synthetic,
            gc_control_consistency: Evidence::Synthetic,
            delete_completion: Evidence::Synthetic,
            ..fake::capabilities(true)
        }
    }

    struct Harness {
        _directory: tempfile::TempDir,
        root: PathBuf,
        store: GcStore,
        provider: FakeProvider,
        repository: RepositoryHandle,
        descriptor: Descriptor,
        root_key: [u8; 32],
    }

    impl Harness {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().to_path_buf();
            Self {
                store: GcStore::open(&root).unwrap(),
                _directory: directory,
                root,
                provider: FakeProvider::new(true),
                repository: fake::repository(),
                descriptor: Descriptor::new("synthetic-descriptor".into(), Some(Strategy::Cas))
                    .unwrap(),
                root_key: [7; 32],
            }
        }
        fn context(&self) -> leases::LeaseContext<'_> {
            leases::LeaseContext {
                root: &self.root,
                connection_id: CONNECTION,
                writer_id: "writer",
                descriptor: &self.descriptor,
                root_key: &self.root_key,
                provider: &self.provider,
                repository: &self.repository,
            }
        }
        fn rows(&self) -> Vec<LeaseIntent> {
            self.store.lease_intents(CONNECTION).unwrap()
        }
        /// The work leases the repository shows, whoever placed them.
        fn work_leases(&self) -> Vec<String> {
            self.provider
                .state
                .lock()
                .unwrap()
                .objects
                .keys()
                .filter(|object| object.starts_with("work-"))
                .cloned()
                .collect()
        }
        /// Counts every object the fake ever created, so a lease that was
        /// placed and given back inside one call is still visible afterwards.
        fn creations(&self) -> u64 {
            self.provider.state.lock().unwrap().next_version
        }
    }

    /// GC29: the lease is confirmed in the repository before the command asks
    /// for any data, and it is gone once the command has finished.
    #[test]
    fn an_export_holds_a_confirmed_lease_while_it_reads_and_gives_it_back() {
        let harness = Harness::new();
        let cancel = Cancellation::default();
        let held = RefCell::new(Vec::new());
        block_on(async {
            let context = harness.context();
            let value = with_export_lease(&context, &cleanup_ready(), NOW, &cancel, async {
                let rows = harness.rows();
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].kind, LeaseKind::Work);
                assert_eq!(rows[0].state, LeaseState::Confirmed);
                assert!(harness.provider.holds(&rows[0].locator.object));
                *held.borrow_mut() = harness.work_leases();
                Ok(7u8)
            })
            .await
            .unwrap();
            assert_eq!(value, 7);
        });
        assert_eq!(held.borrow().len(), 1);
        assert!(harness.rows().is_empty());
        assert!(harness.work_leases().is_empty());
    }

    /// GC29: a delete marker stops the data requests whatever its age is, and
    /// the lease this command placed to find that out is still handed back.
    #[test]
    fn an_export_waits_for_a_delete_marker_however_old_it_is() {
        let harness = Harness::new();
        let cancel = Cancellation::default();
        let marker = lease_object_id(LeaseKind::Deleting, &"a".repeat(32)).unwrap();
        harness
            .provider
            .seed(&marker, ObjectRole::Lease, b"foreign".to_vec());
        let placed = harness.creations();
        let started = AtomicBool::new(false);
        block_on(async {
            let context = harness.context();
            let error =
                with_export_lease(&context, &cleanup_ready(), NOW + 8 * DAY, &cancel, async {
                    started.store(true, Ordering::SeqCst);
                    Ok(())
                })
                .await
                .unwrap_err();
            assert_eq!(error.kind, ErrorKind::Transient);
        });
        assert!(!started.load(Ordering::SeqCst));
        // The wait happened after this command announced itself, and the
        // announcement did not outlive the call.
        assert_eq!(harness.creations(), placed + 1);
        assert!(harness.provider.holds(&marker));
        assert!(harness.rows().is_empty());
        assert!(harness.work_leases().is_empty());
    }

    /// GC29: a body that fails or is cancelled ends the same way. Nothing of
    /// this command is still in flight once it has returned.
    #[test]
    fn an_export_that_fails_or_is_cancelled_still_returns_its_lease() {
        for kind in [ErrorKind::Cancelled, ErrorKind::Corrupt] {
            let harness = Harness::new();
            let cancel = Cancellation::default();
            block_on(async {
                let context = harness.context();
                let error = with_export_lease(&context, &cleanup_ready(), NOW, &cancel, async {
                    Err::<(), _>(ProviderError::new(kind))
                })
                .await
                .unwrap_err();
                assert_eq!(error.kind, kind);
            });
            assert!(harness.rows().is_empty());
            assert!(harness.work_leases().is_empty());
        }
    }

    /// A repository whose removals cannot be trusted never runs a cleanup, so
    /// the command announces nothing and the export proceeds as before.
    #[test]
    fn an_export_announces_nothing_where_cleanup_is_unavailable() {
        let harness = Harness::new();
        let cancel = Cancellation::default();
        let started = AtomicBool::new(false);
        block_on(async {
            let context = harness.context();
            with_export_lease(&context, &fake::capabilities(true), NOW, &cancel, async {
                started.store(true, Ordering::SeqCst);
                Ok(())
            })
            .await
            .unwrap();
        });
        assert!(started.load(Ordering::SeqCst));
        assert!(harness.rows().is_empty());
        assert_eq!(harness.creations(), 0);
    }

    #[test]
    fn request_ids_are_bounded_before_dialog_or_network() {
        assert!(valid_id("connection"));
        assert!(!valid_id(""));
        assert!(!valid_id(&"x".repeat(1025)));
        assert!(!valid_id("bad\0id"));
        assert_eq!(
            archive_name("../snapshot/id"),
            "RisuNest-snapshotid.risunest"
        );
    }
}
