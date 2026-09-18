use super::{
    auth::{SecretBytes, SecretVault},
    capabilities::Capabilities,
    capture::CaptureCatalog,
    connection_store::StoredConnection,
    contract::{Cancellation, ConnectionConfig, SecretRef},
    descriptor,
    fake::{self, FakeProvider, MemoryVault},
    journal::{JobIdentity, TransferJournal},
    packaging::{package_and_upload, PackageLimits, SnapshotMetadata},
    recovery, snapshot_restore,
};
use crate::{
    asset_repository::PayloadCas,
    logical_records::{
        encode_logical_record, encode_logical_record_key, LogicalRecordEnvelope,
        LogicalRecordLocator,
    },
    persistent_store::{
        content_capture::ContentCaptureSink,
        external_apply::{
            ExternalSnapshotApplication, ExternalSnapshotObject, ExternalSnapshotRecord,
        },
        sync_selection::CaptureIdentity,
        PersistentStore,
    },
};
use risunest_external_storage_format::{
    crypto::RecoveryCode,
    format::{library_fingerprint_domain, Descriptor},
};
use serde_json::json;
use std::{collections::BTreeMap, io::Read};
use zeroize::Zeroizing;

#[test]
fn recovery_package_opens_and_applies_a_real_snapshot_without_the_source_vault() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let source = tempfile::tempdir().unwrap();
            let destination = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let scope = library_fingerprint_domain();
            let descriptor =
                Descriptor::new("synthetic-recovery-repository".into(), None)
                    .unwrap();
            let root_key = [7; 32];
            let cancel = Cancellation::default();
            let descriptor_locator = descriptor::upload(
                source.path(),
                &provider,
                &repository,
                &descriptor,
                &root_key,
                &cancel,
            )
            .await
            .unwrap();

            let asset_bytes = b"synthetic referenced asset";
            let asset = PayloadCas::new(source.path())
                .unwrap()
                .prepare_bytes(asset_bytes)
                .unwrap();
            let root_record = encode_logical_record(&LogicalRecordEnvelope::Root {
                value: json!({ "marker": "recovered-snapshot" }),
                owner_heads: vec![],
            })
            .unwrap();
            let root_key_name = encode_logical_record_key(&LogicalRecordLocator::Root).unwrap();
            let capture_directory = source
                .path()
                .join("external-storage")
                .join("captures")
                .join("recovery-capture");
            let capture_objects = source.path().join("external-storage").join("objects");
            let identity = CaptureIdentity {
                store_id: "source-device".into(),
                library_epoch: "synthetic-library".into(),
                generation: "synthetic-generation".into(),
                selection_epoch: "synthetic-selection".into(),
                revision: 1,
            };
            let mut catalog =
                CaptureCatalog::create(&capture_directory, &capture_objects, None).unwrap();
            catalog.begin(&identity, None).unwrap();
            catalog.record(&root_key_name, &root_record.bytes).unwrap();
            catalog
                .reference(&root_key_name, &asset.content_hash, asset.byte_size)
                .unwrap();
            catalog.finish().unwrap();
            let fingerprint = catalog.content_fingerprint(&library_fingerprint_domain()).unwrap();
            let capture = crate::persistent_store::external_capture::CapturedSnapshot {
                id: "recovery-capture".into(),
                identity: identity.clone(),
                catalog,
                projected_records: 1,
                shared: false,
            };
            let mut journal = TransferJournal::open(
                &source.path().join("transfer-journal"),
                JobIdentity {
                    job_id: "recovery-upload".into(),
                    connection_id: "source-connection".into(),
                    repository_id: repository.repository_id.clone(),
                    capture_id: capture.id.clone(),
                    capture: identity.clone(),
                },
            )
            .unwrap();
            let completed = package_and_upload(
                capture,
                Vec::new(),
                source.path(),
                &source.path().join("package-cache"),
                SnapshotMetadata {
                    snapshot_id: "recovery-snapshot".into(),
                    repository_id: descriptor.repository_id.clone(),
                    library_id: identity.library_epoch.clone(),
                    author_device_id: identity.store_id.clone(),
                    created_at_ms: 1,
                    logical_revision: identity.revision as u64,
                    purpose: crate::external_storage::packaging::SnapshotPurpose::SyncState {
                        epoch: "epoch".into(),
                        generation: risunest_sync_wire::head::Sequence::from(1u64),
                        parent_sections: std::collections::BTreeMap::new(),
                    },
                    parent_snapshot_id: None,
                    content_fingerprint: fingerprint,
                },
                &root_key,
                PackageLimits {
                    max_stored_bytes: 128 * 1024,
                    sdk_overhead_bytes: 0,
                    target_plaintext_bytes: 128 * 1024,
                },
                &mut journal,
                &provider,
                &repository,
                &cancel,
            )
            .await
            .unwrap();

            let stored = StoredConnection {
                id: "source-connection".into(),
                config: ConnectionConfig {
                    provider: "webdav".into(),
                    profile: None,
                    endpoint: "https://synthetic.invalid".into(),
                    account_id: "synthetic-account".into(),
                    location: BTreeMap::from([("root".into(), "RisuNest".into())]),
                    oauth_profile: None,
                },
                descriptor: descriptor.clone(),
                descriptor_locator,
                provider_repository_id: repository.repository_id.clone(),
                credential_ref: "unavailable-source-credential".into(),
                root_key_ref: "unavailable-source-root-key".into(),
                capture_policy: None,
                retention_policy: None,
                capabilities: Capabilities::default(),
                created_at_ms: 1,
                last_sync_at_ms: None,
                last_backup_at_ms: None,
            };
            let exported = recovery::export(&stored, &root_key).unwrap();
            let unavailable_source_vault = MemoryVault::default();
            assert!(unavailable_source_vault
                .read(&SecretRef(stored.root_key_ref.clone()))
                .await
                .is_err());
            drop(stored);

            let mut destination_store = PersistentStore::open(destination.path()).unwrap();
            let revision_before_import = destination_store.revision().unwrap();
            let wrong_code = loop {
                let candidate = RecoveryCode::generate().unwrap().expose();
                if candidate.as_str() != exported.code.as_str() {
                    break candidate;
                }
            };
            assert!(recovery::import(&exported.bytes, &wrong_code).is_err());
            assert_eq!(
                destination_store.revision().unwrap(),
                revision_before_import
            );

            let imported = recovery::import(&exported.bytes, &exported.code).unwrap();
            assert_eq!(imported.metadata.descriptor, descriptor);
            assert_eq!(
                imported.metadata.provider_repository_id,
                repository.repository_id
            );
            let destination_vault = MemoryVault::default();
            let destination_key_ref = destination_vault
                .store(&SecretBytes(Zeroizing::new(imported.key.to_vec())))
                .await
                .unwrap();
            let destination_key = destination_vault.read(&destination_key_ref).await.unwrap();
            let destination_key: [u8; 32] = destination_key.0.as_slice().try_into().unwrap();
            assert_eq!(destination_key, root_key);

            let authenticated_descriptor = descriptor::read(
                destination.path(),
                &provider,
                &repository,
                &imported.metadata.descriptor_locator,
                &imported.metadata.descriptor,
                &destination_key,
                &cancel,
            )
            .await
            .unwrap();
            assert_eq!(authenticated_descriptor, descriptor);
            let prepared = snapshot_restore::download_snapshot(
                &completed.reference,
                &destination.path().join("download"),
                &destination_key,
                &provider,
                &repository,
                &cancel,
            )
            .await
            .unwrap();
            assert_eq!(prepared.snapshot_id, "recovery-snapshot");
            assert_eq!(prepared.records.len(), 1);
            assert_eq!(prepared.records[0].key, root_key_name);
            assert_eq!(prepared.records[0].content_hash, root_record.hash);
            assert!(prepared
                .objects
                .iter()
                .any(|object| object.content_hash == asset.content_hash));

            let library_fingerprint: [u8; 32] = hex::decode(&prepared.library_fingerprint)
                .unwrap()
                .try_into()
                .unwrap();
            let records = prepared.records.iter().map(|record| {
                Ok(ExternalSnapshotRecord {
                    key: record.key.clone(),
                    content_hash: record.content_hash.clone(),
                    byte_length: record.byte_length,
                    path: record.path.clone(),
                })
            });
            let objects = prepared.objects.iter().map(|object| {
                Ok(ExternalSnapshotObject {
                    content_hash: object.content_hash.clone(),
                    byte_length: object.byte_length,
                    path: object.path.clone(),
                })
            });
            let application = ExternalSnapshotApplication {
                expected_revision: revision_before_import,
                staging_root: &prepared.staging_root,
                scope_id: &library_fingerprint_domain(),
                fingerprint: &library_fingerprint,
            };
            let replacement = destination_store
                .prepare_external_snapshot_application(&application, records, objects)
                .unwrap();
            destination_store
                .finish_prepared_replace(replacement)
                .unwrap();
            assert_eq!(
                destination_store.materialize(None).unwrap()["marker"],
                "recovered-snapshot"
            );
            let mut restored_asset = PayloadCas::new(destination.path())
                .unwrap()
                .open_object(&asset.content_hash)
                .unwrap()
                .unwrap();
            let mut restored_asset_bytes = Vec::new();
            restored_asset
                .read_to_end(&mut restored_asset_bytes)
                .unwrap();
            assert_eq!(restored_asset_bytes, asset_bytes);
        });
}
